import { test, expect, type APIRequestContext, type Locator, type Page } from '../harness'
import {
  createHash,
  createHmac,
  generateKeyPairSync,
  randomBytes,
  sign,
  type KeyObject,
} from 'node:crypto'
import { mkdirSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

/**
 * Settings → Teams (teams of Peckboard servers), end to end against a REAL
 * team hub: the skills hub binary the Playwright config starts on the
 * shard's PORT+4 (PECKBOARD_TEAM_HUB_URL points the box at it). The spec
 * itself plays "the other server": it holds its own Ed25519 key and talks
 * to the hub directly with the signed-request protocol, the signed
 * membership log, and the invite secret maths (gate, join proof,
 * verification code) from peckboard-team-format.
 *
 *  - A pasted text that is not an invite link: inline error, no request.
 *  - Create a team in the UI: this server is its owner, one member.
 *  - UI invites: the spec joins with the link, the UI shows the same code
 *    the spec computes, approving makes the spec's key a member on the hub.
 *  - UI joins: the spec's team and invite, the UI shows the code, the spec
 *    approves on the hub, "Check again" turns the UI into a member.
 *  - A leaked link: two requests, two different codes, only one matches.
 *
 * Screenshots (light/dark, desktop and 380 px) go to `.tmp-teams-shots/`
 * at the repo root, an untracked scratch dir.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const HUB = process.env.PECKBOARD_E2E_TEAM_HUB ?? ''
const HERE = path.dirname(fileURLToPath(import.meta.url))
const SHOTS = path.resolve(HERE, '..', '..', '..', '.tmp-teams-shots')

test.skip(!HUB, 'no team hub binary (built by scripts/build-local-release.sh)')

// ── the other server: key, signed requests, log records ──────────────────

interface Peer {
  key: string
  priv: KeyObject
}

function newPeer(): Peer {
  const { publicKey, privateKey } = generateKeyPairSync('ed25519')
  const x = publicKey.export({ format: 'jwk' }).x as string
  return { key: Buffer.from(x, 'base64url').toString('base64'), priv: privateKey }
}

const sha256hex = (s: string) => createHash('sha256').update(s).digest('hex')
const signB64 = (p: Peer, msg: string) => sign(null, Buffer.from(msg), p.priv).toString('base64')

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

function solvePow(nonce: string, bits: number): string {
  for (let i = 0; ; i++) {
    const p = String(i)
    if (
      leadingZeroBits(
        createHash('sha256')
          .update(nonce + p)
          .digest(),
      ) >= bits
    )
      return p
  }
}

/** One signed hub request: challenge, proof of work, `X-Peck-*` headers. */
async function hub(
  p: Peer,
  method: 'GET' | 'POST',
  urlPath: string,
  body?: unknown,
): Promise<{ status: number; json: Record<string, unknown> }> {
  const c = (await (
    await fetch(`${HUB}/api/challenge?public_key=${encodeURIComponent(p.key)}`)
  ).json()) as { nonce: string; pow_difficulty: number }
  const raw = body === undefined ? '' : JSON.stringify(body)
  const msg = `peckboard-skills-request-v1\n${method}\n${urlPath}\n${c.nonce}\n${sha256hex(raw)}`
  const res = await fetch(`${HUB}${urlPath}`, {
    method,
    headers: {
      'X-Peck-Key': p.key,
      'X-Peck-Nonce': c.nonce,
      'X-Peck-Pow': solvePow(c.nonce, c.pow_difficulty),
      'X-Peck-Signature': signB64(p, msg),
      'Content-Type': 'application/json',
    },
    body: method === 'GET' ? undefined : raw,
  })
  const text = await res.text()
  return { status: res.status, json: text ? JSON.parse(text) : {} }
}

interface LogRecord {
  seq: number
  prev: string
  at: string
  body: string
  signer: string
  signature: string
}

const logMessage = (r: Pick<LogRecord, 'seq' | 'prev' | 'at' | 'body'>) =>
  `peckboard-team-log-v1\n${r.seq}\n${r.prev}\n${r.at}\n${sha256hex(r.body)}`
const recordHash = (r: LogRecord) => sha256hex(`${logMessage(r)}\n${r.signer}\n${r.signature}`)

function record(p: Peer, seq: number, prev: string, action: Record<string, unknown>): LogRecord {
  const r = { seq, prev, at: new Date().toISOString(), body: JSON.stringify(action) }
  return { ...r, signer: p.key, signature: signB64(p, logMessage(r)) }
}

async function readLog(p: Peer, teamId: string): Promise<LogRecord[]> {
  const res = await hub(p, 'GET', `/api/teams/${teamId}/log`)
  expect(res.status, JSON.stringify(res.json)).toBe(200)
  return res.json.records as LogRecord[]
}

/** The spec founds a team on the hub as `owner`. */
async function hubCreateTeam(owner: Peer, name: string): Promise<string> {
  const r = record(owner, 0, '', {
    type: 'create',
    name,
    founder: owner.key,
    label: 'Spec owner server',
    salt: randomBytes(16).toString('hex'),
  })
  const res = await hub(owner, 'POST', '/api/teams', { record: r })
  expect(res.status, JSON.stringify(res.json)).toBe(201)
  return res.json.team_id as string
}

// ── invites ──────────────────────────────────────────────────────────────

interface Invite {
  teamId: string
  inviteId: string
  secret: string
}

function parseLink(link: string): Invite {
  const [teamId, inviteId, secret] = link.split('#')[1].split('.')
  return { teamId, inviteId, secret }
}

const gate = (secret: string) => sha256hex(`peckboard-team-gate-v1\n${secret}`)
const keyed = (secret: string, ctx: string, teamId: string, key: string) =>
  createHmac('sha256', secret).update(`${ctx}\n${teamId}\n${key}`).digest()

function verificationCode(inv: Invite, key: string): string {
  const n = keyed(inv.secret, 'peckboard-team-verify-v1', inv.teamId, key).readUInt32BE(0) % 1e6
  const pad = (v: number) => String(v).padStart(3, '0')
  return `${pad(Math.floor(n / 1000))} ${pad(n % 1000)}`
}

/** File a join request as `p`, holding the link (a correct proof). */
async function askToJoin(p: Peer, inv: Invite, label: string): Promise<string> {
  const res = await hub(p, 'POST', `/api/teams/${inv.teamId}/join`, {
    invite_id: inv.inviteId,
    gate: gate(inv.secret),
    proof: keyed(inv.secret, 'peckboard-team-join-v1', inv.teamId, p.key).toString('hex'),
    label,
  })
  expect(res.status, JSON.stringify(res.json)).toBe(201)
  return res.json.request_id as string
}

// ── the UI side ──────────────────────────────────────────────────────────

type Auth = { token: string; auth: Record<string, string> }

async function authenticate(request: APIRequestContext): Promise<Auth> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function openTeams(page: Page, token: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto('/settings/teams')
  await expect(page.getByTestId('teams-section')).toBeVisible()
  await expect(page.getByTestId('teams-fingerprint')).toBeVisible()
}

type UiTeam = { id: string; name: string; state: string }

async function uiTeams(request: APIRequestContext, auth: Auth): Promise<UiTeam[]> {
  const res = await request.get('/api/teams', { headers: auth.auth })
  expect(res.ok(), await res.text()).toBeTruthy()
  return ((await res.json()) as { teams: UiTeam[] }).teams
}

const card = (page: Page, id: string) =>
  page.locator(`[data-testid="team-card"][data-team-id="${id}"]`)

const memberRows = (c: Locator) => c.getByTestId('team-members').getByRole('listitem')

async function createInUi(page: Page, name: string) {
  await page.getByTestId('teams-create').or(page.getByTestId('teams-empty-create')).first().click()
  const modal = page.getByTestId('team-create-modal')
  await modal.getByTestId('team-create-name').fill(name)
  await modal.getByTestId('team-create-submit').click()
  await expect(modal).toBeHidden()
}

/** Light + dark, desktop + 380 px, of whatever is on screen now; `focus`
 *  is scrolled into view after each resize. */
async function shots(page: Page, name: string, focus?: Locator) {
  mkdirSync(SHOTS, { recursive: true })
  const original = page.viewportSize() ?? { width: 1280, height: 720 }
  for (const theme of ['light', 'dark']) {
    await page.evaluate((t) => document.documentElement.setAttribute('data-theme', t), theme)
    for (const width of [1280, 380]) {
      await page.setViewportSize({ width, height: 900 })
      await page.waitForTimeout(250)
      if (focus) await focus.evaluate((el) => el.scrollIntoView({ block: 'center' }))
      await page.screenshot({ path: path.join(SHOTS, `${name}-${theme}-${width}.png`) })
    }
  }
  await page.setViewportSize(original)
  await page.evaluate(() => document.documentElement.removeAttribute('data-theme'))
}

test.describe('teams of servers', () => {
  test('a pasted text that is not an invite link shows an inline error and asks nothing', async ({
    page,
    request,
  }) => {
    const auth = await authenticate(request)
    const before = await uiTeams(request, auth)
    await openTeams(page, auth.token)
    if (before.length === 0) {
      await expect(page.getByTestId('teams-empty')).toBeVisible()
      await shots(page, 'empty')
    }
    await page.getByTestId('teams-join').or(page.getByTestId('teams-empty-join')).first().click()
    const modal = page.getByTestId('team-join-modal')
    await modal.getByTestId('team-join-link').fill('https://example.com/some/page')
    await modal.getByTestId('team-join-continue').click()
    await expect(modal.getByTestId('team-join-link-error')).toContainText('not a team invite link')
    // Still on the paste step; nothing was asked and no team appeared.
    await expect(modal.getByTestId('team-join-preview')).toHaveCount(0)
    await expect(modal.getByTestId('team-join-submit')).toHaveCount(0)
    expect((await uiTeams(request, auth)).length).toBe(before.length)
  })

  test('create a team: this server is its owner and the only member', async ({ page, request }) => {
    const auth = await authenticate(request)
    await openTeams(page, auth.token)
    const name = `Create ${Date.now()}`
    await createInUi(page, name)
    const team = (await uiTeams(request, auth)).find((t) => t.name === name)
    expect(team?.state).toBe('member')
    const c = card(page, team!.id)
    await expect(c.getByTestId('team-name')).toHaveText(name)
    await expect(c.getByTestId('team-role')).toHaveText('You are owner')
    await expect(memberRows(c)).toHaveCount(1)
    await expect(memberRows(c).first().getByTestId('team-member-self')).toBeVisible()
    await expect(memberRows(c).first().getByTestId('team-member-role-tag')).toHaveText('Owner')
    await expect(c.getByTestId('team-requests-empty')).toBeVisible()
  })

  test('the UI invites a server: same code on both sides, approving makes it a member', async ({
    page,
    request,
  }) => {
    const auth = await authenticate(request)
    await openTeams(page, auth.token)
    const name = `Invite ${Date.now()}`
    await createInUi(page, name)
    const team = (await uiTeams(request, auth)).find((t) => t.name === name)!
    const c = card(page, team.id)

    await c.getByTestId('team-invite').click()
    const modal = page.getByTestId('team-invite-modal')
    await modal.getByTestId('team-invite-create').click()
    const linkBox = modal.getByTestId('team-invite-link')
    await expect(linkBox).toHaveValue(/\/join#/)
    await expect(modal.getByTestId('team-invite-expiry')).toContainText('Expires')
    await shots(page, 'invite-dialog')
    const link = await linkBox.inputValue()
    expect(link.startsWith(`${HUB}/join#${team.id}.`)).toBeTruthy()
    await modal.getByTestId('team-invite-done').click()
    await expect(c.getByTestId('team-invites').getByRole('listitem')).toHaveCount(1)

    // The coworker's server asks to join with the link.
    const inv = parseLink(link)
    const coworker = newPeer()
    await askToJoin(coworker, inv, 'Coworker laptop')
    const code = verificationCode(inv, coworker.key)

    await page.getByTestId('teams-refresh').click()
    const req = c.getByTestId('team-request')
    await expect(req).toHaveCount(1)
    await expect(req).toContainText('Coworker laptop')
    await expect(req.getByTestId('team-request-code')).toHaveText(code)

    await req.getByTestId('team-request-approve').click()
    const confirm = page.getByTestId('team-approve-confirm')
    await expect(confirm).toContainText(
      `Approve only if the person you invited read you this code: ${code}.`,
    )
    await confirm.getByTestId('team-approve-confirm-button').click()
    await expect(confirm).toBeHidden()
    await expect(memberRows(c)).toHaveCount(2)
    await expect(c.getByTestId('team-requests-empty')).toBeVisible()
    // The invite is used up.
    await expect(c.getByTestId('team-invites')).toHaveCount(0)

    // The hub agrees: the coworker can read the log, and it was added.
    const log = await readLog(coworker, team.id)
    const last = JSON.parse(log[log.length - 1].body) as Record<string, string>
    expect(last).toMatchObject({ type: 'add', key: coworker.key, role: 'member' })
  })

  test('the UI joins a team: it shows the code, and is a member once approved', async ({
    page,
    request,
  }) => {
    const auth = await authenticate(request)
    const owner = newPeer()
    const teamName = `Joined ${Date.now()}`
    const teamId = await hubCreateTeam(owner, teamName)
    const inv: Invite = {
      teamId,
      inviteId: `inv_${randomBytes(6).toString('hex')}`,
      secret: randomBytes(32).toString('base64url'),
    }
    const reg = await hub(owner, 'POST', `/api/teams/${teamId}/invites`, {
      invite_id: inv.inviteId,
      gate: gate(inv.secret),
      expires_at: new Date(Date.now() + 2 * 86_400_000).toISOString(),
    })
    expect(reg.status, JSON.stringify(reg.json)).toBe(201)
    const link = `${HUB}/join#${teamId}.${inv.inviteId}.${inv.secret}`

    await openTeams(page, auth.token)
    await page.getByTestId('teams-join').or(page.getByTestId('teams-empty-join')).first().click()
    const modal = page.getByTestId('team-join-modal')
    await modal.getByTestId('team-join-link').fill(link)
    await modal.getByTestId('team-join-continue').click()
    await expect(modal.getByTestId('team-join-preview-name')).toHaveText(teamName)
    await expect(modal.getByTestId('team-join-preview')).toContainText('1 server in this team')
    await modal.getByTestId('team-join-label').fill('UI server')
    await modal.getByTestId('team-join-submit').click()
    await expect(modal).toBeHidden()

    const pending = page.locator(`[data-testid="team-pending"][data-team-id="${teamId}"]`)
    await expect(pending).toBeVisible()
    await expect(pending).toContainText(
      'Read this code to the person who invited you. They approve the request once it matches.',
    )

    // The owner (the spec) sees the request and computes the same code.
    const listed = await hub(owner, 'GET', `/api/teams/${teamId}/requests`)
    const requests = listed.json.requests as { request_id: string; key: string; label: string }[]
    const mine = requests.find((r) => r.label === 'UI server')!
    expect(mine).toBeTruthy()
    const code = verificationCode(inv, mine.key)
    await expect(pending.getByTestId('team-pending-code')).toHaveText(code)
    await shots(page, 'pending-join', pending)

    // The owner approves: the signed `add` record settling the request.
    const log = await readLog(owner, teamId)
    const r = record(owner, log.length, recordHash(log[log.length - 1]), {
      type: 'add',
      key: mine.key,
      role: 'member',
      label: mine.label,
    })
    const added = await hub(owner, 'POST', `/api/teams/${teamId}/log`, {
      record: r,
      request_id: mine.request_id,
    })
    expect(added.status, JSON.stringify(added.json)).toBe(200)

    await pending.getByTestId('team-pending-check').click()
    await expect(pending).toHaveCount(0)
    const c = card(page, teamId)
    await expect(c.getByTestId('team-role')).toHaveText('You are member')
    await expect(memberRows(c)).toHaveCount(2)
    // A plain member manages nothing: no invites, no requests, no row menus.
    await expect(c.getByTestId('team-invite')).toHaveCount(0)
    await expect(c.getByTestId('team-requests')).toHaveCount(0)
    await expect(
      c.getByTestId('team-members').getByRole('button', { name: 'Row menu' }),
    ).toHaveCount(0)
  })

  test('a leaked link: two requests with different codes, only one matches', async ({
    page,
    request,
  }) => {
    const auth = await authenticate(request)
    const name = `Leak ${Date.now()}`
    const created = await request.post('/api/teams', {
      headers: auth.auth,
      data: { name, label: 'Office server' },
    })
    expect(created.status(), await created.text()).toBe(201)
    const team = (await created.json()) as { id: string }
    const made = await request.post(`/api/teams/${team.id}/invites`, {
      headers: auth.auth,
      data: {},
    })
    expect(made.status(), await made.text()).toBe(201)
    const inv = parseLink(((await made.json()) as { link: string }).link)

    const coworker = newPeer()
    const thief = newPeer()
    await askToJoin(thief, inv, 'laptop')
    await askToJoin(coworker, inv, 'laptop')
    const spoken = verificationCode(inv, coworker.key)

    await openTeams(page, auth.token)
    const c = card(page, team.id)
    const reqs = c.getByTestId('team-request')
    await expect(reqs).toHaveCount(2)
    const codes = await reqs.getByTestId('team-request-code').allTextContents()
    expect(codes).toHaveLength(2)
    expect(codes[0]).not.toBe(codes[1])
    expect(codes.filter((x) => x === spoken)).toHaveLength(1)
    expect(codes).toContain(verificationCode(inv, thief.key))
    await shots(page, 'owner-with-requests', reqs.first())
  })
})
