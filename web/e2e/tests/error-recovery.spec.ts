import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e tests for the error-recovery surfaces:
 *
 *  - ChatView shows a retry pane (not a silent empty conversation) when
 *    the initial events fetch fails, and Retry recovers in place.
 *  - ChatView shows a retry banner when the session-detail / todo
 *    snapshot fetches fail, and Retry clears it.
 *  - The connection banner appears when the WebSocket never
 *    authenticates and disappears once the connection comes up.
 *
 * Fetch failures are simulated with Playwright route interception; the
 * WS case uses routeWebSocket so the page-side socket opens but the
 * server half is under test control.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type AuthBundle = {
  token: string
  authHeader: { Authorization: string }
}

async function authenticate(request: APIRequestContext): Promise<AuthBundle> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

async function seedAuthedSession(
  request: APIRequestContext,
  authHeader: Record<string, string>,
): Promise<{ sessionId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-err-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-err', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'error recovery', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  return { sessionId: session.id }
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

test('failed events fetch shows a retry pane and Retry recovers the chat', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedAuthedSession(request, authHeader)

  // Fail only this session's events fetch — everything else loads.
  const eventsPattern = `**/api/sessions/${sessionId}/events*`
  await page.route(eventsPattern, (route) => route.abort())

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  const errorPane = page.getByTestId('chat-events-error')
  await expect(errorPane).toBeVisible({ timeout: 10_000 })
  // The silent-empty regression: no "No messages yet" while broken.
  await expect(page.locator('.chat-empty')).toHaveCount(0)

  // Heal the network and retry in place — no reload.
  await page.unroute(eventsPattern)
  await errorPane.getByRole('button', { name: 'Retry' }).click()

  await expect(page.locator('.chat-empty')).toBeVisible({ timeout: 10_000 })
  await expect(page.getByTestId('chat-events-error')).toHaveCount(0)
})

test('failed session-detail/todos fetch shows a banner and Retry clears it', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedAuthedSession(request, authHeader)

  // Fail only the todo-snapshot fetch; events + session detail succeed,
  // so the conversation itself renders normally behind the banner.
  const todosPattern = `**/api/sessions/${sessionId}/todos`
  await page.route(todosPattern, (route) => route.abort())

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  await expect(page.locator('.chat-empty')).toBeVisible({ timeout: 10_000 })
  const banner = page.getByTestId('chat-meta-error')
  await expect(banner).toBeVisible({ timeout: 10_000 })

  await page.unroute(todosPattern)
  await banner.getByRole('button', { name: 'Retry' }).click()

  await expect(page.getByTestId('chat-meta-error')).toHaveCount(0, { timeout: 10_000 })
})

test('events arriving over WS during the initial fetch are not dropped', async ({
  request,
  page,
  baseURL,
}) => {
  // Regression: fetchEvents used to wholesale-replace the session's
  // event list with the HTTP snapshot, clobbering any event that was
  // broadcast over the WS while the fetch was in flight.
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedAuthedSession(request, authHeader)

  // Hold the events fetch: grab the real (pre-POST) snapshot right
  // away, then only fulfil once the test releases it.
  let releaseFetch: () => void = () => {}
  const released = new Promise<void>((resolve) => {
    releaseFetch = resolve
  })
  const eventsPattern = `**/api/sessions/${sessionId}/events*`
  await page.route(eventsPattern, async (route) => {
    const snapshot = await route.fetch()
    const body = await snapshot.text()
    await released
    await route.fulfill({ response: snapshot, body })
  })

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  // Wait for the WS to be live (rail status dot) so the broadcast below
  // definitely reaches this client while the fetch is still held.
  await expect(page.locator('.rail-status.online')).toBeVisible({ timeout: 10_000 })

  const postRes = await request.post(`/api/sessions/${sessionId}/events`, {
    headers: authHeader,
    data: { kind: 'user', data: { text: 'arrived mid-fetch' } },
  })
  expect(postRes.ok(), `event post failed: ${await postRes.text()}`).toBeTruthy()

  // Let the WS frame land in the store, then release the stale snapshot.
  await page.waitForTimeout(800)
  releaseFetch()

  await expect(page.locator('.chat-bubble-user')).toContainText('arrived mid-fetch', {
    timeout: 10_000,
  })
})

test('failed attachment upload shows an error chip instead of vanishing', async ({
  request,
  page,
  baseURL,
}) => {
  // Regression: a non-ok upload response was silently ignored — the user
  // picked a file and nothing happened.
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedAuthedSession(request, authHeader)

  await page.route(`**/api/sessions/${sessionId}/attachments`, (route) => route.abort())

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty')).toBeVisible({ timeout: 10_000 })

  await page.locator('input[type="file"]').setInputFiles({
    name: 'notes.txt',
    mimeType: 'text/plain',
    buffer: Buffer.from('hello'),
  })

  const error = page.getByTestId('upload-error')
  await expect(error).toBeVisible({ timeout: 10_000 })
  await expect(error).toContainText('notes.txt')

  // Dismiss clears it.
  await error.getByRole('button', { name: 'Dismiss upload errors' }).click()
  await expect(page.getByTestId('upload-error')).toHaveCount(0)
})

test('connection banner appears while the WS is down and clears on connect', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token } = await authenticate(request)

  // Intercept the app's WebSocket: the page-side socket opens, but the
  // server half is ours — we withhold auth_ok so the app never reaches
  // the connected state, exactly like a server that stopped responding.
  let serverHalf: { send: (data: string) => void } | null = null
  await page.routeWebSocket(/\/ws$/, (ws) => {
    serverHalf = ws
  })

  await loadAppAt(page, token, '/')

  const banner = page.getByTestId('connection-banner')
  await expect(banner).toBeVisible({ timeout: 10_000 })
  await expect(banner).toContainText('Connection lost')

  // "Server" finishes the handshake — the app flips to connected and
  // the banner goes away without a reload.
  expect(serverHalf, 'WS route captured').toBeTruthy()
  serverHalf!.send(JSON.stringify({ type: 'auth_ok', user_id: 'e2e' }))

  await expect(page.getByTestId('connection-banner')).toHaveCount(0, { timeout: 10_000 })
})
