import { test, expect, type APIRequestContext, type Page } from '../harness'
import { createServer, type Server } from 'node:http'
import { execFileSync } from 'node:child_process'
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

/** Serves `index.json` and `skills/<name>/<version>/SKILL.md`, the public
 *  repo layout, from the URL the server was booted with. */
function startIndexStub(): Promise<Server> {
  const server = createServer((req, res) => {
    const url = (req.url ?? '/').split('?')[0]
    if (url.endsWith('/index.json')) {
      res.setHeader('Content-Type', 'application/json')
      res.end(JSON.stringify(INDEX))
      return
    }
    const m = /\/skills\/([^/]+)\/([^/]+)\/SKILL\.md$/.exec(url)
    if (m && m[1] === PUBLIC_SKILL && PUBLIC_VERSIONS.includes(m[2])) {
      res.setHeader('Content-Type', 'text/markdown')
      res.end(skillFile(m[2]))
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
})
