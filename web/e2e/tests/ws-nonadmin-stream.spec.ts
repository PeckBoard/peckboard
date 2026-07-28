import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { WebSocketImpl, type WsMessageEvent } from './ws-compat'

/**
 * E2E for the per-session WS stream gate.
 *
 * The gate used to be a blunt admin check, so a role=`user` account got no
 * live stream at all — not even for its own sessions. Chat looked broken:
 * the optimistic bubble sat on "Sending..." forever and the reply only
 * appeared after a reload. The gate is now an ownership check.
 *
 * Covered here:
 *  1. A non-admin streams a session they own — the reply arrives live.
 *  2. A non-admin subscribing to somebody else's session is refused with a
 *     `subscribe_denied` frame and receives none of its events (guessing
 *     session UUIDs must not open a stream).
 *  3. UI — a refused subscription renders a visible error state instead of
 *     leaving the pending bubble spinning forever.
 */

const ADMIN_USER = 'e2e-user'
const ADMIN_PASS = 'e2e-password-1234'

let cachedAdmin: { token: string; auth: Record<string, string> } | null = null

/** Authenticate as the bootstrap admin once per spec file — the per-IP
 *  login limiter sees the whole suite as a single client. */
async function authenticateAdmin(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  if (cachedAdmin) return cachedAdmin
  const res = await request.post('/api/auth/login', {
    data: { username: ADMIN_USER, password: ADMIN_PASS },
  })
  expect(res.ok(), `admin login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  cachedAdmin = { token, auth: { Authorization: `Bearer ${token}` } }
  return cachedAdmin
}

/** Mint a throwaway role=`user` account and return a bearer token for it. */
async function createNonAdmin(
  request: APIRequestContext,
  adminAuth: Record<string, string>,
  suffix: string,
): Promise<{ token: string; auth: Record<string, string> }> {
  const username = `ws-gate-${suffix}-${Date.now()}`
  const password = 'ws-gate-password-1234'
  const created = await request.post('/api/users', {
    headers: adminAuth,
    data: { username, password, role: 'user' },
  })
  expect(created.ok(), `create user failed: ${await created.text()}`).toBeTruthy()

  const res = await request.post('/api/auth/login', { data: { username, password } })
  expect(res.ok(), `login as ${username} failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

/** Folder creation is admin-only, so the folder is always seeded with the
 *  admin's token; `owner` only decides who owns the session inside it. */
async function seedSession(
  request: APIRequestContext,
  adminAuth: Record<string, string>,
  owner: Record<string, string>,
  name: string,
): Promise<string> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-wsgate-'))
  const folderRes = await request.post('/api/folders', {
    headers: adminAuth,
    data: { name: `ws-gate-${name}`, path: folderPath },
  })
  expect(folderRes.ok(), `folder create failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: owner,
    data: { name, folder_id: folder.id },
  })
  expect(sessionRes.ok(), `session create failed: ${await sessionRes.text()}`).toBeTruthy()
  return ((await sessionRes.json()) as { id: string }).id
}

type Collected = {
  events: { sessionId: string; kind: string }[]
  denied: { sessionId: string; reason: string }[]
}

/** Open a WS, authenticate, subscribe to `sessionId`, and collect `event`
 *  and `subscribe_denied` frames until `untilKind` arrives or the timeout
 *  hits. A denial resolves immediately — nothing more will ever arrive. */
function collectFrames(
  baseURL: string,
  token: string,
  sessionId: string,
  untilKind: string,
  timeoutMs: number,
): Promise<Collected> {
  const wsUrl = baseURL.replace(/^http/, 'ws') + '/ws'
  const ws = new WebSocketImpl(wsUrl)
  const collected: Collected = { events: [], denied: [] }

  return new Promise((resolve, reject) => {
    const finish = () => {
      ws.close()
      resolve(collected)
    }
    const timer = setTimeout(finish, timeoutMs)

    ws.addEventListener('error', (err) => {
      clearTimeout(timer)
      ws.close()
      reject(new Error(`WS error: ${String(err)}`))
    })
    ws.addEventListener('open', () => {
      ws.send(JSON.stringify({ type: 'auth', token }))
    })
    ws.addEventListener('message', (msg: WsMessageEvent) => {
      const frame = JSON.parse(String(msg.data))
      if (frame.type === 'auth_ok') {
        ws.send(JSON.stringify({ type: 'subscribe', session_id: sessionId }))
        return
      }
      if (frame.type === 'subscribe_denied') {
        collected.denied.push({ sessionId: frame.session_id, reason: frame.reason })
        return
      }
      if (frame.type !== 'event') return
      collected.events.push({ sessionId: frame.session_id, kind: frame.event.kind })
      if (frame.session_id === sessionId && frame.event.kind === untilKind) {
        clearTimeout(timer)
        finish()
      }
    })
  })
}

test('a non-admin streams a session they own', async ({ request, baseURL }) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { auth: adminAuth } = await authenticateAdmin(request)
  const { token, auth } = await createNonAdmin(request, adminAuth, 'own')
  const sessionId = await seedSession(request, adminAuth, auth, 'owned by the non-admin')

  const frames = collectFrames(baseURL!, token, sessionId, 'agent-end', 15_000)
  // Let the socket auth + subscribe before the turn starts emitting.
  await new Promise((r) => setTimeout(r, 500))

  const sendRes = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth,
    data: { text: 'hello', model: 'mock:happy-path' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  const { events, denied } = await frames
  expect(denied, `own session was refused: ${JSON.stringify(denied)}`).toHaveLength(0)
  expect(events.some((f) => f.sessionId === sessionId && f.kind === 'agent-end')).toBeTruthy()
})

test("a non-admin is refused another user's session and told so", async ({ request, baseURL }) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { auth: adminAuth } = await authenticateAdmin(request)
  const { token } = await createNonAdmin(request, adminAuth, 'other')
  const adminSessionId = await seedSession(request, adminAuth, adminAuth, 'owned by the admin')

  const frames = collectFrames(baseURL!, token, adminSessionId, 'agent-end', 6_000)
  await new Promise((r) => setTimeout(r, 500))

  const sendRes = await request.post(`/api/sessions/${adminSessionId}/message`, {
    headers: adminAuth,
    data: { text: 'hello', model: 'mock:happy-path' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  const { events, denied } = await frames
  // Refused, and told about it rather than left waiting in silence.
  expect(denied.map((d) => d.sessionId)).toContain(adminSessionId)
  expect(denied[0].reason.length).toBeGreaterThan(0)
  const leaked = events.filter((f) => f.sessionId === adminSessionId)
  expect(leaked, `stream leaked: ${JSON.stringify(leaked)}`).toHaveLength(0)
})

async function loadSession(page: Page, token: string, sessionId: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(`/sessions/${sessionId}`)
}

test('a refused subscription shows an error state instead of hanging', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { auth: adminAuth } = await authenticateAdmin(request)
  const { token } = await createNonAdmin(request, adminAuth, 'ui')
  const adminSessionId = await seedSession(
    request,
    adminAuth,
    adminAuth,
    'admin session for the UI check',
  )

  await loadSession(page, token, adminSessionId)

  // The refusal must reach the UI as a visible error…
  const banner = page.getByTestId('chat-stream-denied')
  await expect(banner).toBeVisible({ timeout: 10_000 })
  await expect(banner).toContainText('Live updates unavailable')

  // …and an optimistic bubble must not sit on "Sending..." forever, since
  // the `user` event that would clear it can never arrive on this stream.
  await page.locator('textarea.input-textarea').fill('does this hang?')
  await page.locator('textarea.input-textarea').press('Enter')
  await expect(page.getByTestId('chat-pending-status')).toHaveText('Sent — reply not shown live', {
    timeout: 10_000,
  })
})
