import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E for the REST-layer per-session ownership gate.
 *
 * Sessions used to be readable/writable by ANY logged-in user over REST —
 * `list_sessions`, `get_session`, the events/transcript route, and
 * `send_message` took no identity into account at all. A non-admin could
 * list, open, and post into another user's chat session by guessing (or
 * simply being shown) its UUID. `require_session_access` + the owner
 * filter on `list_sessions` close that; this spec locks the fix in.
 *
 * Covered here:
 *  2. A non-admin gets 404 (not the transcript, not a leak-y 403) on
 *     GET session / GET events / POST message / GET queue / POST queue for
 *     a session they don't own.
 *  3. UI — opening another user's session by id shows the chat's generic
 *     load-error state instead of rendering the transcript.
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
  const username = `rest-gate-${suffix}-${Date.now()}`
  const password = 'rest-gate-password-1234'
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
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-restgate-'))
  const folderRes = await request.post('/api/folders', {
    headers: adminAuth,
    data: { name: `rest-gate-${name}`, path: folderPath },
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

test("a non-admin's session list never includes another user's session", async ({ request }) => {
  const { auth: adminAuth } = await authenticateAdmin(request)
  const { auth } = await createNonAdmin(request, adminAuth, 'lister')
  const adminSessionId = await seedSession(request, adminAuth, adminAuth, 'admin only chat')

  const listRes = await request.get('/api/sessions', { headers: auth })
  expect(listRes.ok(), `list failed: ${await listRes.text()}`).toBeTruthy()
  const { items } = (await listRes.json()) as { items: { id: string }[] }
  expect(items.map((s) => s.id)).not.toContain(adminSessionId)
})

test("a non-admin cannot read or post into another user's session over REST", async ({
  request,
}) => {
  const { auth: adminAuth } = await authenticateAdmin(request)
  const { auth } = await createNonAdmin(request, adminAuth, 'rw')
  const adminSessionId = await seedSession(request, adminAuth, adminAuth, 'admin only chat 2')

  const getRes = await request.get(`/api/sessions/${adminSessionId}`, { headers: auth })
  expect(getRes.status(), 'GET session must 404, not leak the transcript').toBe(404)

  const eventsRes = await request.get(`/api/sessions/${adminSessionId}/events`, { headers: auth })
  expect(eventsRes.status(), 'GET events must 404').toBe(404)

  const messageRes = await request.post(`/api/sessions/${adminSessionId}/message`, {
    headers: auth,
    data: { text: 'pwned', model: 'mock:happy-path' },
  })
  expect(messageRes.status(), 'POST message must 404, not spawn a turn').toBe(404)

  // The queued message is text that gets sent into the session, so the
  // queue routes are session reads/writes too — they live in
  // routes/notifications.rs and were missed by the first pass at this gate.
  const queueGet = await request.get(`/api/sessions/${adminSessionId}/queue`, { headers: auth })
  expect(queueGet.status(), 'GET queue must 404').toBe(404)

  const queuePost = await request.post(`/api/sessions/${adminSessionId}/queue`, {
    headers: auth,
    data: { text: 'queued pwn' },
  })
  expect(queuePost.status(), 'POST queue must 404, not queue a turn').toBe(404)

  // Same for the folder move — it cancels the running agent and repoints
  // the workspace the session runs in.
  const folderMove = await request.post(`/api/sessions/${adminSessionId}/folder`, {
    headers: auth,
    data: { target_folder_id: 'whatever' },
  })
  expect(folderMove.status(), 'POST folder must 404').toBe(404)

  // The per-turn usage breakdown includes a snippet of each user prompt,
  // so it is session content too (routes/usage).
  const turnsRes = await request.get(`/api/usage/sessions/${adminSessionId}/turns`, {
    headers: auth,
  })
  expect(turnsRes.status(), 'GET usage turns must 404').toBe(404)
})

async function loadSession(page: Page, token: string, sessionId: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(`/sessions/${sessionId}`)
}

test("opening another user's session by id never renders its transcript", async ({
  request,
  page,
}) => {
  const { auth: adminAuth } = await authenticateAdmin(request)
  const { token } = await createNonAdmin(request, adminAuth, 'ui')
  const sessionName = 'admin session for the REST UI check'
  const adminSessionId = await seedSession(request, adminAuth, adminAuth, sessionName)

  await loadSession(page, token, adminSessionId)

  // Whatever the app does to recover from the denied load (currently: it
  // falls back to the Sessions list), two things must always hold: no
  // composer for the denied session ever renders — there is no way to
  // post into it — and the admin's session name never appears on the
  // page, so its existence isn't leaked either.
  await expect(page.locator('textarea.input-textarea')).not.toBeVisible()
  await expect(page.getByText(sessionName)).toHaveCount(0)
})
