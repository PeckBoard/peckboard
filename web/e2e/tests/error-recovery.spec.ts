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

/**
 * Store-level "failed fetch must not render as an empty state" coverage.
 * Each store used to swallow a non-2xx / network failure into empty data, so
 * the UI claimed "there is nothing here" when the truth was "we could not load
 * it". Every case below asserts the error + a Retry that actually refetches.
 */

async function seedProject(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  name: string,
): Promise<{ projectId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-err-proj-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-err-${name}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    // worker_count=0 keeps the orchestrator from spawning workers that would
    // churn the board underneath the assertions.
    headers: authHeader,
    data: { name, folder_id: folder.id, worker_count: 0, workflow: 'task' },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }
  return { projectId: project.id }
}

test('a failed usage leg shows an error strip with Retry, never a $0.00 total', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token } = await authenticate(request)

  // Only the per-session rollup fails; the other legs still load, so this is
  // the partial-failure case the dashboard used to render as "$0.00 spent".
  const sessionsUsage = '**/api/usage/sessions*'
  await page.route(sessionsUsage, (route) => route.abort())

  await loadAppAt(page, token, '/usage')

  const banner = page.getByTestId('usage-fetch-error')
  await expect(banner).toBeVisible({ timeout: 10_000 })
  await expect(page.getByTestId('usage-totals')).not.toContainText('$0.00')
  await expect(page.getByTestId('usage-panel-sessions-error')).toBeVisible()
  await expect(page.locator('[data-testid="usage-panel-sessions"] .usage-panel-empty')).toHaveCount(
    0,
  )

  await page.unroute(sessionsUsage)
  await banner.getByRole('button', { name: 'Retry' }).click()

  await expect(page.getByTestId('usage-fetch-error')).toHaveCount(0, { timeout: 10_000 })
  await expect(page.getByTestId('usage-panel-sessions-error')).toHaveCount(0)
})

test('a failed project list shows an error with Retry, not “No projects yet”', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  await seedProject(request, authHeader, 'error recovery projects')

  const projectsPattern = '**/api/projects'
  await page.route(projectsPattern, (route) => route.abort())

  await loadAppAt(page, token, '/projects')

  const error = page.getByTestId('projects-error')
  await expect(error).toBeVisible({ timeout: 10_000 })
  await expect(page.locator('.list-view-empty', { hasText: 'No projects yet' })).toHaveCount(0)

  await page.unroute(projectsPattern)
  await error.getByRole('button', { name: 'Retry' }).click()

  await expect(page.getByTestId('projects-error')).toHaveCount(0, { timeout: 10_000 })
  await expect(
    page.locator('.list-view-name', { hasText: 'error recovery projects' }).first(),
  ).toBeVisible()
})

test('a failed cards fetch shows an error with Retry, not empty board columns', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const { projectId } = await seedProject(request, authHeader, 'error recovery board')
  const cardRes = await request.post(`/api/projects/${projectId}/cards`, {
    headers: authHeader,
    data: { title: 'Recovered Card', description: '', step: 'backlog', priority: 2 },
  })
  expect(cardRes.ok(), `seed card failed: ${await cardRes.text()}`).toBeTruthy()

  const cardsPattern = `**/api/projects/${projectId}/cards`
  await page.route(cardsPattern, (route) => route.abort())

  await loadAppAt(page, token, `/projects/${projectId}`)

  const error = page.getByTestId('kanban-cards-error')
  await expect(error).toBeVisible({ timeout: 10_000 })
  // The regression: five columns each claiming “No cards in …”.
  await expect(page.locator('.kanban-cards-empty')).toHaveCount(0)

  await page.unroute(cardsPattern)
  await error.getByRole('button', { name: 'Retry' }).click()

  await expect(page.getByTestId('kanban-cards-error')).toHaveCount(0, { timeout: 10_000 })
  await expect(page.locator('.kanban-card').filter({ hasText: 'Recovered Card' })).toBeVisible()
})

test('a failed “load older” page keeps the affordance and offers Retry', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedAuthedSession(request, authHeader)

  // A first page as long as the store's page size is what makes it believe
  // more history exists; synthesising it beats POSTing 200 real events.
  const firstPage = Array.from({ length: 200 }, (_, i) => ({
    id: `seed-${i + 1}`,
    session_id: sessionId,
    seq: i + 1,
    ts: 1_700_000_000_000 + i,
    kind: 'user',
    data: { text: `seeded message ${i + 1}` },
  }))
  let olderFails = true
  await page.route(`**/api/sessions/${sessionId}/events*`, async (route) => {
    if (!route.request().url().includes('before_seq')) {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(firstPage),
      })
      return
    }
    await route.fulfill({
      status: olderFails ? 500 : 200,
      contentType: 'application/json',
      body: olderFails ? '{"error":"boom"}' : '[]',
    })
  })

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  const loadOlder = page.getByTestId('chat-load-older')
  await expect(loadOlder).toBeVisible({ timeout: 10_000 })
  await loadOlder.click()

  // The regression: a 500 counted as a short page, so the store marked
  // history exhausted and the control disappeared until a full reload.
  const error = page.getByTestId('chat-load-older-error')
  await expect(error).toBeVisible({ timeout: 10_000 })

  olderFails = false
  await page.getByTestId('chat-load-older-retry').click()

  await expect(page.getByTestId('chat-load-older-error')).toHaveCount(0, { timeout: 10_000 })
  // An empty page that actually arrived is the honest end-of-history signal.
  await expect(page.getByTestId('chat-load-older')).toHaveCount(0)
})

test('WS connects when the token lives only in sessionStorage (no Remember me)', async ({
  request,
  page,
  baseURL,
}) => {
  // Regression: the WS open handler read localStorage directly instead
  // of the shared getToken() helper, so a non-"Remember me" login (token
  // in sessionStorage only) never sent an auth frame and the app never
  // came online for the whole session.
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedAuthedSession(request, authHeader)

  await page.addInitScript((injectedToken) => {
    sessionStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(`/sessions/${sessionId}`)

  await expect(page.locator('.rail-status.online')).toBeVisible({ timeout: 10_000 })
})
