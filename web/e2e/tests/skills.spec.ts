import { test, expect, type APIRequestContext, type Page } from '../harness'
import {
  createServer,
  type IncomingHttpHeaders,
  type IncomingMessage,
  type Server,
  type ServerResponse,
} from 'node:http'
import { execFileSync } from 'node:child_process'
import { createHash, createPublicKey, randomBytes, verify } from 'node:crypto'
import { mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Skills, end to end through the UI:
 *  - Settings → Skills: create a local skill and a group, search, edit (new
 *    version) and diff two versions.
 *  - Browse the public catalog (a mock index this spec serves on the port
 *    the server's PECKBOARD_SKILLS_INDEX_URL points at), subscribe to a
 *    pinned version, then move the pin to Latest; the `?subscribe=` deep
 *    link opens the same skill.
 *  - Give a session a group from the chat 3-dot menu's "Skills" entry.
 *  - Team memory: the proprietary-content guard's 422 shows its findings
 *    under the note field; a clean note saves.
 *  - Publish a local skill: the guard's 422 lists body lines and metadata
 *    findings; a clean skill is pushed to a team repo (a local bare git repo
 *    behind a file:// url).
 *  - Publish to Peckboard Public: the same stub plays the hub (challenge,
 *    proof of work, ed25519 signature check, name ownership) — a name
 *    another install owns shows the plain-words 409; a free one is published
 *    with a link.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const STUB_PORT = Number(process.env.PECKBOARD_E2E_SKILLS_PORT ?? '4447')

const PUBLIC_SKILL = 'pr-review-checklist'
const PUBLIC_VERSIONS = ['1.1.0', '1.0.0']

function skillFile(version: string): string {
  return [
    '---',
    `name: ${PUBLIC_SKILL}`,
    'description: Walk a pull request through a review checklist',
    `version: ${version}`,
    'tags: [review, git]',
    'groups: [pr-review]',
    '---',
    `# PR review checklist v${version}`,
    '',
    '1. Read the description.',
    version === '1.1.0' ? '2. Run the tests locally.' : '2. Skim the diff.',
    '',
  ].join('\n')
}

const INDEX = {
  schema: 1,
  generated_at: '2026-11-01T00:00:00Z',
  skills: [
    {
      name: PUBLIC_SKILL,
      description: 'Walk a pull request through a review checklist',
      tags: ['review', 'git'],
      groups: ['pr-review'],
      versions: PUBLIC_VERSIONS,
      latest: PUBLIC_VERSIONS[0],
    },
  ],
  groups: [
    {
      slug: 'pr-review',
      name: 'PR review',
      description: 'Everything for reviewing a pull request',
      skills: [PUBLIC_SKILL],
    },
  ],
}

/** What the stub hub has accepted: skill name → the publishing install's
 *  public key. A name owned by another install is answered `name_taken`. */
const HUB_OWNERS = new Map<string, string>()
const HUB_NONCES = new Set<string>()
const HUB_POW_BITS = 4
const HUB_TAKEN = `e2e-hub-taken-${Date.now()}`
HUB_OWNERS.set(HUB_TAKEN, Buffer.alloc(32, 7).toString('base64'))

function leadingZeroBits(hash: Buffer): number {
  let n = 0
  for (const b of hash) {
    if (b === 0) {
      n += 8
      continue
    }
    return n + Math.clz32(b) - 24
  }
  return n
}

/** `POST /api/publish` of the hub contract: one-time nonce, proof of work,
 *  ed25519 signature over the publish message, name ownership. */
function hubPublish(raw: string): { status: number; body: unknown } {
  const b = JSON.parse(raw) as Record<string, string>
  if (!HUB_NONCES.delete(b.nonce))
    return { status: 400, body: { error: 'nonce', code: 'nonce_invalid' } }
  const pow = createHash('sha256')
    .update(`${b.nonce}${b.pow ?? ''}`)
    .digest()
  if (leadingZeroBits(pow) < HUB_POW_BITS)
    return { status: 400, body: { error: 'pow', code: 'pow_required' } }
  const digest = createHash('sha256').update(b.skill_md, 'utf8').digest('hex')
  const key = createPublicKey({
    key: {
      kty: 'OKP',
      crv: 'Ed25519',
      x: Buffer.from(b.public_key, 'base64').toString('base64url'),
    },
    format: 'jwk',
  })
  const msg = Buffer.from(`peckboard-skills-publish-v1\n${b.nonce}\n${digest}`, 'utf8')
  if (!verify(null, msg, key, Buffer.from(b.signature, 'base64')))
    return { status: 401, body: { error: 'bad signature', code: 'bad_signature' } }
  const name = /^name:\s*(\S+)/m.exec(b.skill_md)?.[1] ?? ''
  const version = /^version:\s*(\S+)/m.exec(b.skill_md)?.[1] ?? ''
  const owner = HUB_OWNERS.get(name)
  if (owner && owner !== b.public_key)
    return { status: 409, body: { error: 'taken', code: 'name_taken' } }
  HUB_OWNERS.set(name, b.public_key)
  // Publishing `<name>@<handle>` publishes to that fork.
  HUB_FORK_VERSIONS.get(name)?.set(version, b.skill_md)
  return {
    status: 201,
    body: { name, version, url: `https://skills.peckboard.com/s/${name}` },
  }
}

// ── The stub hub's fork / merge-request side (peckboard-skills-hub/API.md) ──

const HUB_ME = 'e2e-me'
const ALICE_KEY = Buffer.alloc(32, 9).toString('base64')
/** This PeckBoard's key, learnt from its first signed request. */
let MY_KEY = ''
/** The stub also plays the original's author, so one PeckBoard can walk
 *  both sides of a merge request. */
let HUB_ACT_AS_OWNER = false
/** fork name → version → SKILL.md */
const HUB_FORK_VERSIONS = new Map<string, Map<string, string>>()
type StubMr = Record<string, unknown> & { id: number; state: string; target_owner_key: string }
const HUB_MRS: StubMr[] = []
/** Every signed request that verified: method, path, JSON body. */
const HUB_SIGNED: { method: string; path: string; body: unknown }[] = []

const newestFirst = (vs: string[]) =>
  [...vs].sort((a, b) => b.localeCompare(a, undefined, { numeric: true }))

function forkLatest(fork: string): { version: string; text: string } {
  const versions = HUB_FORK_VERSIONS.get(fork)!
  const version = newestFirst([...versions.keys()])[0]
  return { version, text: versions.get(version)! }
}

/** A whole-file unified diff — enough for the UI to render. */
function unifiedDiff(a: string, b: string): string {
  const x = a.trimEnd().split('\n')
  const y = b.trimEnd().split('\n')
  return [
    '--- a/SKILL.md',
    '+++ b/SKILL.md',
    `@@ -1,${x.length} +1,${y.length} @@`,
    ...x.map((l) => `-${l}`),
    ...y.map((l) => `+${l}`),
    '',
  ].join('\n')
}

/** The `X-Peck-*` signed headers: single-use nonce, proof of work, an
 *  ed25519 signature over method, path, nonce and sha256(body). */
function verifySigned(method: string, path: string, headers: IncomingHttpHeaders, raw: string) {
  const h = (k: string) => String(headers[k] ?? '')
  const nonce = h('x-peck-nonce')
  if (!HUB_NONCES.delete(nonce)) return false
  const pow = createHash('sha256')
    .update(`${nonce}${h('x-peck-pow')}`)
    .digest()
  if (leadingZeroBits(pow) < HUB_POW_BITS) return false
  const key = createPublicKey({
    key: {
      kty: 'OKP',
      crv: 'Ed25519',
      x: Buffer.from(h('x-peck-key'), 'base64').toString('base64url'),
    },
    format: 'jwk',
  })
  const digest = createHash('sha256').update(raw, 'utf8').digest('hex')
  const msg = Buffer.from(`peckboard-skills-request-v1\n${method}\n${path}\n${nonce}\n${digest}`)
  if (!verify(null, msg, key, Buffer.from(h('x-peck-signature'), 'base64'))) return false
  MY_KEY = h('x-peck-key')
  HUB_SIGNED.push({ method, path, body: raw ? JSON.parse(raw) : null })
  return true
}

function json(res: ServerResponse, status: number, body: unknown) {
  res.statusCode = status
  res.setHeader('Content-Type', 'application/json')
  res.end(JSON.stringify(body))
}

/** Handles, skill details / history / diff, forks and merge requests.
 *  Returns false for anything else. */
function hubCollab(
  req: IncomingMessage,
  url: string,
  query: string | undefined,
  res: ServerResponse,
) {
  const path = decodeURIComponent(url)
  const params = new URLSearchParams(query)
  const signed = (handle: (body: Record<string, unknown>) => [number, unknown]) => {
    let raw = ''
    req.on('data', (c: Buffer) => (raw += c.toString('utf8')))
    req.on('end', () => {
      if (!verifySigned(req.method ?? '', url, req.headers, raw))
        return json(res, 401, { error: 'bad signature', code: 'bad_signature' })
      const [status, body] = handle(raw ? (JSON.parse(raw) as Record<string, unknown>) : {})
      json(res, status, body)
    })
    return true
  }
  if (path.startsWith('/api/publishers/')) {
    json(res, 200, { public_key: path.slice(16), handle: HUB_ME })
    return true
  }
  const sk = /^\/api\/skills\/([^/]+)(\/history|\/forks|\/diff)?$/.exec(path)
  if (sk && req.method === 'GET') {
    const [, name, sub] = sk
    const isFork = HUB_FORK_VERSIONS.has(name)
    if (name !== PUBLIC_SKILL && !isFork) {
      json(res, 404, { error: 'no such skill', code: 'not_found' })
    } else if (sub === '/history') {
      json(res, 200, [
        {
          commit: 'c2'.padEnd(40, '0'),
          parents: ['c1'.padEnd(40, '0')],
          author_key: ALICE_KEY,
          author_handle: 'alice',
          message: 'Run the tests locally',
          at: '2026-11-02T00:00:00Z',
          tag: 'v1.1.0',
        },
        {
          commit: 'c1'.padEnd(40, '0'),
          parents: [],
          author_key: ALICE_KEY,
          author_handle: 'alice',
          message: 'First version',
          at: '2026-11-01T00:00:00Z',
          tag: 'v1.0.0',
        },
      ])
    } else if (sub === '/forks') {
      json(
        res,
        200,
        [...HUB_FORK_VERSIONS.keys()].map((fork) => ({
          fork,
          owner_handle: HUB_ME,
          owner_key: MY_KEY,
          latest: forkLatest(fork).version,
          ahead: 1,
          behind: 0,
        })),
      )
    } else if (sub === '/diff') {
      const text = (rev: string) =>
        PUBLIC_VERSIONS.includes(rev) ? skillFile(rev) : skillFile('1.1.0')
      json(res, 200, {
        from: params.get('from'),
        to: params.get('to'),
        diff: unifiedDiff(text(params.get('from') ?? ''), text(params.get('to') ?? '')),
      })
    } else if (isFork) {
      const versions = newestFirst([...HUB_FORK_VERSIONS.get(name)!.keys()])
      json(res, 200, {
        name,
        owner_handle: HUB_ME,
        owner_key: MY_KEY,
        parent: PUBLIC_SKILL,
        latest: versions[0],
        versions,
        description: 'fork',
        tags: [],
        groups: [],
      })
    } else {
      json(res, 200, {
        name,
        owner_handle: 'alice',
        owner_key: HUB_ACT_AS_OWNER ? MY_KEY : ALICE_KEY,
        parent: null,
        latest: PUBLIC_VERSIONS[0],
        versions: PUBLIC_VERSIONS,
        description: INDEX.skills[0].description,
        tags: ['review', 'git'],
        groups: ['pr-review'],
      })
    }
    return true
  }
  if (path === '/api/fork' && req.method === 'POST')
    return signed((b) => {
      const fork = `${String(b.name)}@${HUB_ME}`
      if (HUB_FORK_VERSIONS.has(fork)) return [409, { error: 'forked', code: 'already_forked' }]
      const text = skillFile('1.1.0').replace(`name: ${PUBLIC_SKILL}`, `name: ${fork}`)
      HUB_FORK_VERSIONS.set(fork, new Map([['1.1.0', text]]))
      return [201, { fork, url: `https://skills.peckboard.com/s/${fork}` }]
    })
  if (path === '/api/merge-requests' && req.method === 'POST')
    return signed((b) => {
      const fork = String(b.fork)
      const now = new Date().toISOString()
      const mr: StubMr = {
        id: HUB_MRS.length + 1,
        target: PUBLIC_SKILL,
        fork,
        title: b.title,
        description: b.description,
        state: 'open',
        author_key: MY_KEY,
        author_handle: HUB_ME,
        target_owner_key: ALICE_KEY,
        base_commit: 'c2'.padEnd(40, '0'),
        head_commit: 'f1'.padEnd(40, '0'),
        mergeable: true,
        diff: unifiedDiff(skillFile('1.1.0'), forkLatest(fork).text),
        comments: [],
        created_at: now,
        updated_at: now,
        merged_commit: null,
        merged_version: null,
      }
      HUB_MRS.push(mr)
      return [201, mr]
    })
  if (path === '/api/merge-requests' && req.method === 'GET') {
    const key = params.get('key')
    const state = params.get('state') ?? 'open'
    const list = HUB_MRS.filter(
      (m) =>
        (params.get('role') === 'owner' ? m.target_owner_key === key : m.author_key === key) &&
        (state === 'all' || m.state === state),
    ).map((m) => {
      // List entries carry no diff or comments.
      const rest: Record<string, unknown> = { ...m }
      delete rest.diff
      delete rest.comments
      return rest
    })
    json(res, 200, list)
    return true
  }
  const mm = /^\/api\/merge-requests\/(\d+)(\/merge|\/close|\/comments)?$/.exec(path)
  if (mm) {
    const mr = HUB_MRS.find((m) => m.id === Number(mm[1]))
    if (!mr) {
      json(res, 404, { error: 'no such merge request', code: 'not_found' })
      return true
    }
    if (!mm[2]) {
      json(res, 200, mr)
      return true
    }
    return signed((b) => {
      if (mm[2] === '/merge') {
        Object.assign(mr, {
          state: 'merged',
          merged_commit: 'm1'.padEnd(40, '0'),
          merged_version: b.publish ? '1.2.0' : null,
        })
      } else if (mm[2] === '/close') {
        mr.state = 'closed'
      } else {
        const c = {
          id: 1,
          author_handle: HUB_ME,
          author_key: MY_KEY,
          body: b.body,
          at: new Date().toISOString(),
        }
        ;(mr.comments as unknown[]).push(c)
        return [201, c]
      }
      return [200, mr]
    })
  }
  return false
}

/** Serves `index.json` and `skills/<name>/<version>/SKILL.md`, the public
 *  repo layout, from the URL the server was booted with — plus the hub's
 *  `/api/challenge` + `/api/publish`, since publishing goes to the same
 *  base URL. */
function startIndexStub(): Promise<Server> {
  const server = createServer((req, res) => {
    const [url, query] = (req.url ?? '/').split('?')
    if (url.endsWith('/index.json')) {
      res.setHeader('Content-Type', 'application/json')
      res.end(JSON.stringify(INDEX))
      return
    }
    if (url.endsWith('/api/challenge')) {
      const nonce = randomBytes(16).toString('base64')
      HUB_NONCES.add(nonce)
      expect(new URLSearchParams(query).get('public_key')).toBeTruthy()
      res.setHeader('Content-Type', 'application/json')
      res.end(JSON.stringify({ nonce, expires_in: 60, pow_difficulty: HUB_POW_BITS }))
      return
    }
    if (url.endsWith('/api/publish') && req.method === 'POST') {
      let raw = ''
      req.on('data', (c: Buffer) => (raw += c.toString('utf8')))
      req.on('end', () => {
        const out = hubPublish(raw)
        res.statusCode = out.status
        res.setHeader('Content-Type', 'application/json')
        res.end(JSON.stringify(out.body))
      })
      return
    }
    if (hubCollab(req, url, query, res)) return
    const m = /\/skills\/([^/]+)\/([^/]+)\/SKILL\.md$/.exec(url)
    if (m && m[1] === PUBLIC_SKILL && PUBLIC_VERSIONS.includes(m[2])) {
      res.setHeader('Content-Type', 'text/markdown')
      res.end(skillFile(m[2]))
      return
    }
    const forkText = m ? HUB_FORK_VERSIONS.get(decodeURIComponent(m[1]))?.get(m[2]) : undefined
    if (forkText) {
      res.setHeader('Content-Type', 'text/markdown')
      res.end(forkText)
      return
    }
    res.statusCode = 404
    res.end('not found')
  })
  return new Promise((resolve) => server.listen(STUB_PORT, '127.0.0.1', () => resolve(server)))
}

type Auth = { token: string; auth: Record<string, string> }

async function authenticate(request: APIRequestContext): Promise<Auth> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

type Source = { id: string; kind: string; name: string; last_error: string | null }

async function listSources(request: APIRequestContext, auth: Record<string, string>) {
  const res = await request.get('/api/skill-sources', { headers: auth })
  expect(res.ok(), `list sources failed: ${await res.text()}`).toBeTruthy()
  const body = (await res.json()) as Source[] | { sources: Source[] }
  return Array.isArray(body) ? body : body.sources
}

/** A git repo the team source can clone and push to: a bare origin seeded
 *  with one commit on `main`. */
function makeTeamRepo(): string {
  const root = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-skills-team-'))
  const bare = path.join(root, 'team.git')
  const work = path.join(root, 'work')
  const git = (cwd: string, ...args: string[]) =>
    execFileSync('git', ['-c', 'user.name=e2e', '-c', 'user.email=e2e@example.invalid', ...args], {
      cwd,
      stdio: 'pipe',
    })
  git(root, 'init', '--bare', '--initial-branch=main', bare)
  git(root, 'clone', bare, work)
  writeFileSync(path.join(work, 'README.md'), '# Team skills\n')
  git(work, 'checkout', '-B', 'main')
  git(work, 'add', 'README.md')
  git(work, 'commit', '-m', 'init')
  git(work, 'push', 'origin', 'main')
  return bare
}

test.describe('skills', () => {
  let stub: Server

  test.beforeAll(async ({ request }) => {
    stub = await startIndexStub()
    // The public source was seeded at boot, before this stub listened, so
    // its first sync failed; sync it now that the index is reachable.
    const { auth } = await authenticate(request)
    const pub = (await listSources(request, auth)).find((s) => s.kind === 'public')
    expect(pub, 'public skill source seeded at boot').toBeTruthy()
    const sync = await request.post(`/api/skill-sources/${pub!.id}/sync`, { headers: auth })
    expect(sync.ok(), `public sync failed: ${await sync.text()}`).toBeTruthy()
  })

  test.afterAll(async () => {
    stub.closeAllConnections()
    await new Promise<void>((resolve) => stub.close(() => resolve()))
  })

  test('create a local skill and a group, search, edit to a new version and diff', async ({
    request,
    page,
  }) => {
    const { token } = await authenticate(request)
    const name = `e2e-local-${Date.now()}`
    await loadAt(page, token, '/settings/skills')
    const root = page.getByTestId('settings-skills')
    await expect(root).toBeVisible()

    // 1. New local skill.
    await page.getByTestId('skill-new').click()
    const editor = page.getByTestId('skill-editor-modal')
    await editor.getByTestId('skill-editor-save').click()
    // Empty form: the fields say what is missing.
    await expect(editor.getByTestId('skill-editor-name-error')).toBeVisible()
    await editor.getByTestId('skill-editor-name').fill(name)
    await editor.getByTestId('skill-editor-description').fill('Say hello politely')
    await editor.getByTestId('skill-editor-tags').fill('greeting, e2e')
    await editor.getByTestId('skill-editor-body').fill('# Hello\n\nGreet the user by name.\n')
    await editor.getByTestId('skill-editor-save').click()
    await expect(editor).toHaveCount(0)
    await expect(page.getByTestId(`skill-row-${name}`)).toBeVisible()

    // 2. Search narrows the installed list.
    await page.getByTestId('skills-search').fill('no-such-skill-zzz')
    await expect(page.getByTestId(`skill-row-${name}`)).toHaveCount(0)
    await page.getByTestId('skills-search').fill('greeting')
    await expect(page.getByTestId(`skill-row-${name}`)).toBeVisible()

    // 3. A group holding the skill, membership picked from the combobox.
    await page.getByTestId('skills-tab-groups').click()
    await page.getByTestId('skill-group-new').click()
    const groupModal = page.getByTestId('skill-group-modal')
    const groupSlug = `e2e-group-${Date.now()}`
    await groupModal.getByTestId('skill-group-name').fill('E2E Group')
    await groupModal.getByTestId('skill-group-slug').fill(groupSlug)
    await groupModal.getByTestId('skill-group-skills').click()
    await page.getByTestId('skill-group-skills-search').fill(name)
    await page.getByTestId(`skill-group-skills-option-${name}`).click()
    await expect(groupModal.getByTestId(`skill-group-skills-chip-${name}`)).toBeVisible()
    await groupModal.getByTestId('skill-group-save').click()
    await expect(groupModal).toHaveCount(0)
    await expect(page.getByTestId(`skill-group-row-${groupSlug}`)).toBeVisible()
    await expect(page.locator('.list-view-row', { hasText: 'E2E Group' })).toContainText('1 skill')

    // 4. Edit → minor bump → v1.1.0, then diff it against v1.0.0.
    await page.getByTestId('skills-tab-installed').click()
    await page.getByTestId(`skill-row-${name}`).click()
    const detail = page.getByTestId('skill-detail-modal')
    await expect(detail).toContainText('v1.0.0')
    await detail.getByTestId('skill-detail-edit').click()
    await expect(editor.getByTestId('skill-editor-body')).toHaveValue(/Greet the user by name/)
    await editor
      .getByTestId('skill-editor-body')
      .fill('# Hello\n\nGreet the user by name, warmly.\n')
    await editor.getByTestId('skill-editor-bump').selectOption('minor')
    await editor.getByTestId('skill-editor-save').click()
    await expect(editor).toHaveCount(0)
    const row = page.locator('.list-view-row', { has: page.getByTestId(`skill-row-${name}`) })
    await expect(row).toContainText('v1.1.0')

    await page.getByTestId(`skill-row-${name}`).click()
    await expect(detail.getByTestId('skill-version-body')).toContainText('warmly')
    await detail.getByTestId('skill-diff-against').selectOption('1.0.0')
    const diff = detail.getByTestId('skill-version-diff')
    await expect(diff).toContainText('-Greet the user by name.')
    await expect(diff).toContainText('+Greet the user by name, warmly.')
  })

  test('browse the public catalog, subscribe to a pinned version, then follow Latest', async ({
    request,
    page,
  }) => {
    const { token } = await authenticate(request)
    await loadAt(page, token, '/settings/skills')
    await page.getByTestId('skills-tab-browse').click()
    await page.getByTestId('skills-search').fill('review')
    await page.getByTestId(`skill-row-${PUBLIC_SKILL}`).click()

    const modal = page.getByTestId('skill-catalog-modal')
    const versionSelect = modal.getByTestId('skill-version-select')
    // Real versions from the index, plus "Latest".
    await expect(versionSelect.locator('option')).toHaveCount(PUBLIC_VERSIONS.length + 1)
    await versionSelect.selectOption('1.0.0')
    await modal.getByTestId('skill-subscribe').click()
    await expect(modal).toHaveCount(0)

    await page.getByTestId('skills-tab-installed').click()
    const row = page.locator('.list-view-row', {
      has: page.getByTestId(`skill-row-${PUBLIC_SKILL}`),
    })
    await expect(row).toContainText('v1.0.0 (pinned)')

    // Move the pin to Latest from the detail modal.
    await page.getByTestId(`skill-row-${PUBLIC_SKILL}`).click()
    const detail = page.getByTestId('skill-detail-modal')
    await expect(detail.getByTestId('skill-version-select')).toHaveValue('1.0.0')
    await detail.getByTestId('skill-version-select').selectOption('')
    await expect(row).toContainText('v1.1.0 (latest)')
    await expect(detail.getByTestId('skill-version-select')).toHaveValue('')
    await page.keyboard.press('Escape')

    // The peckboard.com link opens the same skill in Browse.
    await page.goto(`/settings?tab=skills&subscribe=${PUBLIC_SKILL}`)
    await expect(page.getByTestId('skills-search')).toHaveValue(PUBLIC_SKILL)
    await expect(page.getByTestId('skill-catalog-modal')).toBeVisible()
    await expect(page.getByTestId('skill-subscribe-reason')).toContainText('Already subscribed')
  })

  test('give a session a skill group from the session menu', async ({ request, page }) => {
    const { token, auth } = await authenticate(request)
    const slug = `e2e-session-group-${Date.now()}`
    const groupRes = await request.post('/api/skill-groups', {
      headers: auth,
      data: { slug, name: 'Session group', description: 'for the session test', skills: [] },
    })
    expect(groupRes.ok(), `create group failed: ${await groupRes.text()}`).toBeTruthy()
    const group = (await groupRes.json()) as { id: string }

    const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-skills-'))
    const folderRes = await request.post('/api/folders', {
      headers: auth,
      data: { name: `e2e-skills-${Date.now()}`, path: folderPath },
    })
    expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
    const folder = (await folderRes.json()) as { id: string }
    const sessionRes = await request.post('/api/sessions', {
      headers: auth,
      data: { name: 'skills flow', folder_id: folder.id, model: 'mock:echo' },
    })
    expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
    const session = (await sessionRes.json()) as { id: string }

    await loadAt(page, token, `/sessions/${session.id}`)
    await expect(page.locator('.chat-toolbar')).toBeVisible({ timeout: 10_000 })
    await page.getByTestId('chat-toolbar-menu').click()
    await page.getByTestId('chat-menu-skills').click()
    const modal = page.getByTestId('session-skills-modal')
    await expect(modal).toBeVisible()
    await modal.getByTestId('session-skills-picker').click()
    await page.getByTestId('session-skills-picker-search').fill('Session group')
    await page.getByTestId(`session-skills-picker-option-group:${group.id}`).click()
    await expect(modal.getByTestId(`session-skills-picker-chip-group:${group.id}`)).toBeVisible()
    await modal.getByTestId('session-skills-save').click()
    await expect(modal).toHaveCount(0)

    const got = await request.get(`/api/sessions/${session.id}/skills`, { headers: auth })
    expect(got.ok()).toBeTruthy()
    expect(((await got.json()) as { groups: string[] }).groups).toEqual([group.id])

    // Same entry on the tab strip's context menu, showing the saved pick.
    await page.locator('.tab-opened.tab-active').click({ button: 'right' })
    await page.locator('.context-menu button', { hasText: /^Skills$/ }).click()
    await expect(modal.getByTestId(`session-skills-picker-chip-group:${group.id}`)).toBeVisible()
    await page.keyboard.press('Escape')

    await request.delete(`/api/sessions/${session.id}`, { headers: auth })
  })

  test('team memory: the guard rejects proprietary content and lists the lines', async ({
    request,
    page,
  }) => {
    const { token } = await authenticate(request)
    const repo = makeTeamRepo()
    const sourceName = `e2e-team-${Date.now()}`

    await loadAt(page, token, '/settings/skills')
    await page.getByTestId('skills-tab-sources').click()
    await page.getByTestId('skill-source-new').click()
    const srcModal = page.getByTestId('skill-source-modal')
    await srcModal.getByTestId('skill-source-name').fill(sourceName)
    await srcModal.getByTestId('skill-source-url').fill(repo)
    await srcModal.getByTestId('skill-source-save').click()
    await expect(srcModal).toHaveCount(0)
    await expect(page.getByTestId(`skill-source-row-${sourceName}`)).toBeVisible()

    await page.getByTestId('skills-tab-memory').click()
    await page.getByTestId('skills-memory-source').selectOption({ label: sourceName })
    await page.getByTestId('skill-memory-new').click()
    const noteModal = page.getByTestId('skill-memory-modal')
    await noteModal.getByTestId('skill-memory-title').fill('Deploy notes')
    await noteModal
      .getByTestId('skill-memory-body')
      .fill(
        [
          'How we deploy:',
          'ssh into pg01.prod.internal first',
          'then export AWS_KEY=AKIAABCDEFGHIJKLMNOP',
        ].join('\n'),
      )
    await noteModal.getByTestId('skill-memory-save').click()
    const findings = noteModal.getByTestId('skill-memory-findings')
    await expect(findings).toBeVisible()
    // One finding per offending line (lines 2 and 3), each naming its line.
    await expect(noteModal.getByTestId('skill-memory-finding').first()).toContainText(/^Line \d+/)
    await expect(findings).toContainText('Line 2')
    await expect(findings).toContainText('Line 3')
    // Nothing was written.
    await expect(noteModal).toBeVisible()

    // A clean note goes through.
    await noteModal
      .getByTestId('skill-memory-body')
      .fill('How we deploy:\n\nRun the release script, then watch the dashboard.\n')
    await expect(findings).toHaveCount(0)
    await noteModal.getByTestId('skill-memory-save').click()
    await expect(noteModal).toHaveCount(0)
    await expect(page.getByTestId('skill-memory-row-deploy-notes')).toBeVisible()
  })

  test('publish a local skill: the guard lists findings, a clean one lands in the team repo', async ({
    request,
    page,
  }) => {
    const { token, auth } = await authenticate(request)
    const repo = makeTeamRepo()
    const sourceName = `e2e-publish-team-${Date.now()}`
    const srcRes = await request.post('/api/skill-sources', {
      headers: auth,
      data: { kind: 'team', name: sourceName, url: `file://${repo}` },
    })
    expect(srcRes.ok(), `add team source failed: ${await srcRes.text()}`).toBeTruthy()

    // Built at runtime so no secret-shaped literal sits in the repo.
    const fakeToken = 'ghp_' + 'x'.repeat(36)
    const stamp = Date.now()
    const dirty = `e2e-pub-dirty-${stamp}`
    const clean = `e2e-pub-clean-${stamp}`
    for (const [name, description, body] of [
      [dirty, 'Deploy steps for pg01.prod.internal', `# Deploy\n\nexport GH_TOKEN=${fakeToken}\n`],
      [clean, 'Write a friendly changelog entry', '# Changelog\n\nOne line per change.\n'],
    ]) {
      const res = await request.post('/api/skills', {
        headers: auth,
        data: { name, description, tags: [], groups: [], body },
      })
      expect(res.ok(), `create ${name} failed: ${await res.text()}`).toBeTruthy()
    }

    await loadAt(page, token, '/settings/skills')
    const modal = page.getByTestId('skill-publish-modal')

    // 1. From the row menu: the guard blocks it and names each problem.
    const dirtyRow = page.locator('.list-view-row', { has: page.getByTestId(`skill-row-${dirty}`) })
    await dirtyRow.locator('.list-view-menu').click()
    await page.getByTestId(`skill-publish-${dirty}`).click()
    await modal.getByTestId('skill-publish-target').selectOption({ label: `${sourceName} (team)` })
    await modal.getByTestId('skill-publish-submit').click()
    const findings = modal.getByTestId('skill-publish-findings')
    await expect(findings).toBeVisible()
    // The description is metadata, not a body line; the token is body line 3.
    await expect(findings).toContainText('Metadata: internal_host')
    await expect(findings).toContainText('Line 3: github_token')
    await expect(findings).not.toContainText(fakeToken)
    await expect(modal.getByTestId('skill-publish-result')).toHaveCount(0)
    await page.keyboard.press('Escape')
    await expect(modal).toHaveCount(0)

    // 2. From the detail modal: a clean skill is committed and pushed.
    await page.getByTestId(`skill-row-${clean}`).click()
    await page.getByTestId('skill-detail-publish').click()
    await modal.getByTestId('skill-publish-target').selectOption({ label: `${sourceName} (team)` })
    await modal.getByTestId('skill-publish-submit').click()
    await expect(modal.getByTestId('skill-publish-result')).toContainText(
      `Published to ${sourceName}.`,
    )
    await expect(modal.getByTestId('skill-publish-findings')).toHaveCount(0)
    const pushed = execFileSync(
      'git',
      ['--git-dir', repo, 'show', `main:skills/${clean}/1.0.0/SKILL.md`],
      { encoding: 'utf8' },
    )
    expect(pushed).toContain('One line per change.')
    await modal.getByTestId('skill-publish-close').click()
    await expect(modal).toHaveCount(0)
  })

  test('publish to Peckboard Public: a taken name says so, a free one is published', async ({
    request,
    page,
  }) => {
    const { token, auth } = await authenticate(request)
    const pub = (await listSources(request, auth)).find((s) => s.kind === 'public')!
    const mine = `e2e-hub-mine-${Date.now()}`
    for (const name of [HUB_TAKEN, mine]) {
      const res = await request.post('/api/skills', {
        headers: auth,
        data: {
          name,
          description: 'Write a friendly changelog entry',
          tags: [],
          groups: [],
          body: '# Changelog\n\nOne line per change.\n',
        },
      })
      expect(res.ok(), `create ${name} failed: ${await res.text()}`).toBeTruthy()
    }

    await loadAt(page, token, '/settings/skills')
    const modal = page.getByTestId('skill-publish-modal')
    const publicLabel = `${pub.name} (public catalog)`

    // 1. Another install owns the name: the hub's 409 reads in plain words.
    await page.getByTestId(`skill-row-${HUB_TAKEN}`).click()
    await page.getByTestId('skill-detail-publish').click()
    await modal.getByTestId('skill-publish-target').selectOption({ label: publicLabel })
    await modal.getByTestId('skill-publish-submit').click()
    await expect(modal.getByTestId('skill-publish-error')).toContainText(
      `Another PeckBoard already published a skill called ${HUB_TAKEN}. Rename yours to publish it.`,
    )
    await expect(modal.getByTestId('skill-publish-result')).toHaveCount(0)
    await page.keyboard.press('Escape')
    await expect(modal).toHaveCount(0)
    await page.keyboard.press('Escape')

    // 2. A free name: signed, proof of work done, published with a link.
    await page.getByTestId(`skill-row-${mine}`).click()
    await page.getByTestId('skill-detail-publish').click()
    await modal.getByTestId('skill-publish-target').selectOption({ label: publicLabel })
    await modal.getByTestId('skill-publish-submit').click()
    const result = modal.getByTestId('skill-publish-result')
    await expect(result).toContainText('Published to Peckboard Public.')
    await expect(result).toContainText('within a minute')
    await expect(result).not.toContainText('pull request')
    await expect(modal.getByTestId('skill-publish-link')).toHaveAttribute(
      'href',
      `https://skills.peckboard.com/s/${mine}`,
    )
    // The stub verified the signature and recorded this install as owner.
    expect(HUB_OWNERS.get(mine)).toBeTruthy()
    await modal.getByTestId('skill-publish-close').click()
    await expect(modal).toHaveCount(0)
  })

  test('fork and edit, ask the author to merge, then merge it from Merge requests', async ({
    request,
    page,
  }) => {
    const { token } = await authenticate(request)
    const fork = `${PUBLIC_SKILL}@${HUB_ME}`
    await loadAt(page, token, '/settings/skills')

    // 1. Browse → a public skill someone else owns → Fork and edit opens the
    //    fork's local copy in the editor.
    await page.getByTestId('skills-tab-browse').click()
    await page.getByTestId(`skill-row-${PUBLIC_SKILL}`).first().click()
    const catalog = page.getByTestId('skill-catalog-modal')
    await expect(catalog.getByTestId('skill-catalog-tab-history')).toBeVisible()
    await catalog.getByTestId('skill-catalog-tab-history').click()
    await expect(catalog.getByTestId('skill-hub-history')).toContainText('Run the tests locally')
    await catalog.getByTestId('skill-catalog-tab-overview').click()
    await expect(catalog.getByTestId('skill-fork')).toHaveText(/Fork and edit|Edit my fork/)
    await catalog.getByTestId('skill-fork').click()
    const editor = page.getByTestId('skill-editor-modal')
    await expect(editor.getByTestId('skill-editor-body')).toHaveValue(/Run the tests locally/)
    await editor
      .getByTestId('skill-editor-body')
      .fill('# PR review checklist\n\n1. Read the description.\n2. Run the tests and the linter.\n')
    await editor.getByTestId('skill-editor-save').click()
    await expect(editor).toHaveCount(0)
    expect(HUB_SIGNED.some((s) => s.method === 'POST' && s.path === '/api/fork')).toBeTruthy()

    // 2. The fork is installed; "Ask the author to merge" publishes the edit to
    //    the fork and opens the merge request.
    await page.getByTestId('skills-tab-installed').click()
    await page.getByTestId(`skill-row-${fork}`).click()
    await page.getByTestId('skill-detail-ask-merge').click()
    const ask = page.getByTestId('skill-ask-merge-modal')
    await ask.getByTestId('skill-mr-submit').click()
    await expect(ask.getByTestId('skill-mr-title-error')).toBeVisible()
    await ask.getByTestId('skill-mr-title').fill('Also run the linter')
    await ask.getByTestId('skill-mr-description').fill('Catches style issues before review.')
    await ask.getByTestId('skill-mr-submit').click()
    await expect(ask.getByTestId('skill-ask-merge-done')).toBeVisible()
    expect(forkLatest(fork).text).toContain('Run the tests and the linter.')
    const mr = HUB_MRS.find((m) => m.fork === fork && m.state === 'open')!
    expect(mr.title).toBe('Also run the linter')
    await ask.getByTestId('skill-mr-close').click()

    // 3. Merge requests → Outgoing lists it.
    await page.getByTestId('skills-tab-merge-requests').click()
    await page.getByTestId('skill-mr-role-outgoing').click()
    await expect(page.getByTestId(`skill-mr-row-${mr.id}`)).toContainText('Also run the linter')

    // 4. The original's author (the stub plays them) sees it as Incoming,
    //    with a badge, and merges it as a new minor version.
    HUB_ACT_AS_OWNER = true
    mr.target_owner_key = MY_KEY
    await page.goto('/settings/skills?tab=merge-requests')
    await expect(page.getByTestId('skills-mr-badge')).toContainText('1')
    await page.getByTestId(`skill-mr-row-${mr.id}`).click()
    const detail = page.getByTestId('skill-mr-modal')
    await expect(detail.getByTestId('skill-mr-diff')).toContainText('Run the tests and the linter.')
    await expect(detail.getByTestId('skill-mr-not-mergeable')).toHaveCount(0)
    await expect(detail.getByTestId('skill-mr-publish')).toBeChecked()
    await detail.getByTestId('skill-mr-bump').selectOption('minor')
    await detail.getByTestId('skill-mr-merge').click()
    await expect(detail.getByTestId('skill-mr-notice')).toHaveText(
      'Merged and published as v1.2.0.',
    )
    await expect(detail.getByTestId('skill-mr-state-badge')).toHaveText('Merged')
    const merge = HUB_SIGNED.find((s) => s.path === `/api/merge-requests/${mr.id}/merge`)
    expect(merge?.body).toEqual({ publish: true, bump: 'minor' })
    HUB_ACT_AS_OWNER = false
  })
})
