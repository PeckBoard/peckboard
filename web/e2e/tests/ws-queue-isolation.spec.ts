import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { WebSocketImpl, type WsMessageEvent } from './ws-compat'

/**
 * E2E for the `queue` WS frame's per-session gate.
 *
 * `queue` frames used to be fanned out as GLOBAL events — every connected
 * client got them, and the payload carried the queued message TEXT. A
 * non-admin with any socket open therefore saw the private text (and the
 * session id) of another user's queued message, even though `Subscribe`
 * on that session is refused. The frames now route through the same
 * per-client subscription gate every session event uses.
 *
 * Covered here: a busy admin session queues a message; the owner's
 * subscribed socket gets the `queue` frame, a non-admin's socket gets
 * none of it — and never the text.
 */

const ADMIN_USER = 'e2e-user'
const ADMIN_PASS = 'e2e-password-1234'

/** Text that must never reach the non-admin's socket. */
const SECRET = 'queue-isolation-secret-payload'

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
): Promise<{ token: string; auth: Record<string, string> }> {
  const username = `ws-queue-${Date.now()}`
  const password = 'ws-queue-password-1234'
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
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-wsqueue-'))
  const folderRes = await request.post('/api/folders', {
    headers: adminAuth,
    data: { name: `ws-queue-${name}-${Date.now()}`, path: folderPath },
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

type Collector = {
  /** Every frame the socket received, verbatim. */
  frames: Record<string, unknown>[]
  close: () => void
}

/** Open an authenticated socket, subscribe to `sessionId`, and record every
 *  frame it receives. Resolves once the subscribe has been sent (the server
 *  acks a successful subscribe with nothing — only a refusal is a frame). */
function openSocket(baseURL: string, token: string, sessionId: string): Promise<Collector> {
  const ws = new WebSocketImpl(baseURL.replace(/^http/, 'ws') + '/ws')
  const frames: Record<string, unknown>[] = []

  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('WS auth timed out')), 10_000)
    ws.addEventListener('error', (err) => {
      clearTimeout(timer)
      ws.close()
      reject(new Error(`WS error: ${String(err)}`))
    })
    ws.addEventListener('open', () => {
      ws.send(JSON.stringify({ type: 'auth', token }))
    })
    ws.addEventListener('message', (msg: WsMessageEvent) => {
      const frame = JSON.parse(String(msg.data)) as Record<string, unknown>
      frames.push(frame)
      if (frame.type === 'auth_ok') {
        ws.send(JSON.stringify({ type: 'subscribe', session_id: sessionId }))
        clearTimeout(timer)
        resolve({ frames, close: () => ws.close() })
      }
    })
  })
}

const queueFrames = (c: Collector, sessionId: string) =>
  c.frames.filter((f) => f.type === 'queue' && f.session_id === sessionId)

test("a non-admin never sees another user's queue frames", async ({ request, baseURL }) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { auth: adminAuth } = await authenticateAdmin(request)
  const { token: userToken, auth: userAuth } = await createNonAdmin(request, adminAuth)

  const adminSessionId = await seedSession(request, adminAuth, adminAuth, 'admin queue session')
  const userSessionId = await seedSession(request, adminAuth, userAuth, 'non-admin own session')

  // Owner socket (admin, subscribed to its own session) and an ordinary
  // connected non-admin socket, subscribed only to a session it owns.
  const owner = await openSocket(baseURL!, (await authenticateAdmin(request)).token, adminSessionId)
  const bystander = await openSocket(baseURL!, userToken, userSessionId)
  // Let both subscribes register server-side before the turn starts.
  await new Promise((r) => setTimeout(r, 500))

  try {
    // `mock:ask` parks waiting on stdin — the session stays busy.
    const first = await request.post(`/api/sessions/${adminSessionId}/message`, {
      headers: adminAuth,
      data: { text: 'stay busy', model: 'mock:ask' },
    })
    expect(first.ok(), `first send failed: ${await first.text()}`).toBeTruthy()
    expect(((await first.json()) as { status: string }).status).toBe('started')

    await expect
      .poll(
        () =>
          owner.frames.some(
            (f) =>
              f.type === 'event' &&
              (f.event as { kind?: string } | undefined)?.kind === 'agent-start',
          ),
        { timeout: 10_000, message: 'agent-start on the owner socket' },
      )
      .toBeTruthy()

    // Second send lands mid-turn → parks in the durable queue.
    const second = await request.post(`/api/sessions/${adminSessionId}/message`, {
      headers: adminAuth,
      data: { text: SECRET, model: 'mock:echo' },
    })
    expect(second.ok(), `second send failed: ${await second.text()}`).toBeTruthy()
    expect(((await second.json()) as { status: string }).status).toBe('queued')

    // The owner still gets the frame — the queue pill depends on it.
    await expect
      .poll(() => queueFrames(owner, adminSessionId).length, {
        timeout: 10_000,
        message: 'queue frame on the owner socket',
      })
      .toBeGreaterThan(0)

    // Give any (buggy) global fan-out time to reach the bystander.
    await new Promise((r) => setTimeout(r, 1_000))

    const leaked = queueFrames(bystander, adminSessionId)
    expect(leaked, `queue frame leaked: ${JSON.stringify(leaked)}`).toHaveLength(0)
    const textLeak = bystander.frames.filter((f) => JSON.stringify(f).includes(SECRET))
    expect(textLeak, `message text leaked: ${JSON.stringify(textLeak)}`).toHaveLength(0)
  } finally {
    owner.close()
    bystander.close()
    await request.post(`/api/sessions/${adminSessionId}/interrupt`, { headers: adminAuth })
  }
})
