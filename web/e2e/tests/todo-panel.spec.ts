import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e test for the chat-session todo/task panel.
 *
 * Drives the real React app: logs in via the API, injects the token into
 * localStorage so the app boots authenticated, opens a session, and sends a
 * message backed by the `mock:todo` scenario. That scenario emits a TodoWrite
 * tool call normalized into a `todo` event whose snapshot is
 *   done: "Write the parser", in_progress: "Wire up the route", pending: "Add tests".
 *
 * Asserts the panel groups items by Pending / In Progress / Done with the right
 * markers (done struck through, in_progress emphasized + shown via activeForm),
 * then POSTs a second `todo` event over the same WS broadcast path the real
 * provider uses and asserts the panel re-buckets live (latest snapshot wins).
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type AuthBundle = {
  token: string
  authHeader: { Authorization: string }
}

async function authenticate(request: APIRequestContext): Promise<AuthBundle> {
  // The server auto-bootstraps the admin from PECKBOARD_BOOTSTRAP_*
  // env vars at first start (see playwright.config.ts); we just log in.
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
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-todo-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-todo', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'todo smoke', folder_id: folder.id },
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

test('chat todo panel renders the snapshot grouped by status and updates live', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedAuthedSession(request, authHeader)

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  // App booted authenticated and the chat surface rendered.
  await expect(page.locator('.chat-empty').or(page.locator('.chat-bubble').first())).toBeVisible({
    timeout: 10_000,
  })

  // No panel before any todo snapshot exists (empty state stays out of the way).
  await expect(page.getByTestId('todo-panel')).toHaveCount(0)

  // Trigger the scripted todo snapshot through the real route.
  const sendRes = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'track some work', model: 'mock:todo' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  // Panel appears with a 1-of-3-done progress summary.
  const panel = page.getByTestId('todo-panel')
  await expect(panel).toBeVisible({ timeout: 10_000 })
  await expect(page.getByTestId('todo-panel-count')).toHaveText('1/3 done')

  const items = page.getByTestId('todo-item')
  await expect(items).toHaveCount(3)

  // Done item: original content, struck through.
  const done = page.locator('[data-testid="todo-item"][data-status="done"]')
  await expect(done).toHaveCount(1)
  await expect(done).toContainText('Write the parser')
  await expect(done.locator('.todo-item-text')).toHaveCSS('text-decoration-line', 'line-through')

  // In-progress item: shown via its activeForm ("Wiring up the route").
  const inProgress = page.locator('[data-testid="todo-item"][data-status="in_progress"]')
  await expect(inProgress).toHaveCount(1)
  await expect(inProgress).toContainText('Wiring up the route')

  // Pending item.
  const pending = page.locator('[data-testid="todo-item"][data-status="pending"]')
  await expect(pending).toHaveCount(1)
  await expect(pending).toContainText('Add tests')

  // Live update: emit a fresh snapshot over the same WS broadcast path the real
  // provider uses (latest `todo` event wins). Move "Add tests" to in_progress
  // and "Wire up the route" to done.
  const updateRes = await request.post(`/api/sessions/${sessionId}/events`, {
    headers: authHeader,
    data: {
      kind: 'todo',
      // A real `todo` event carries the backend's normalized status tokens
      // (pending / in_progress / done), so mirror that wire shape here.
      data: {
        todos: [
          { content: 'Write the parser', status: 'done', activeForm: 'Writing the parser' },
          { content: 'Wire up the route', status: 'done', activeForm: 'Wiring up the route' },
          { content: 'Add tests', status: 'in_progress', activeForm: 'Adding tests' },
        ],
      },
    },
  })
  expect(updateRes.ok(), `update event failed: ${await updateRes.text()}`).toBeTruthy()

  // Panel re-buckets live: 2 done, the pending group is gone, in-progress now
  // shows "Adding tests".
  await expect(page.getByTestId('todo-panel-count')).toHaveText('2/3 done')
  await expect(page.locator('[data-testid="todo-item"][data-status="done"]')).toHaveCount(2)
  await expect(page.locator('[data-testid="todo-item"][data-status="pending"]')).toHaveCount(0)
  await expect(page.locator('[data-testid="todo-item"][data-status="in_progress"]')).toContainText(
    'Adding tests',
  )
})

test('clearing the session removes the todo panel from the chat view', async ({
  request,
  page,
  baseURL,
}) => {
  // Regression: the chat-side `loadedTodos` snapshot was set on session
  // mount and never reset on clear, so the panel kept showing the stale
  // pre-clear list even though the server had no events left.
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedAuthedSession(request, authHeader)

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  // Seed a todo snapshot so the panel renders.
  const sendRes = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'track some work', model: 'mock:todo' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  const panel = page.getByTestId('todo-panel')
  await expect(panel).toBeVisible({ timeout: 10_000 })
  await expect(page.getByTestId('todo-item')).toHaveCount(3)

  // Clear the session through the public API — the same endpoint both the
  // chat toolbar menu and the tab context menu post to.
  const clearRes = await request.post(`/api/sessions/${sessionId}/clear`, { headers: authHeader })
  expect(clearRes.ok(), `clear failed: ${await clearRes.text()}`).toBeTruthy()

  // The backend now broadcasts `session-cleared`, so the panel disappears
  // live — no reload required. Before this fix the cached snapshot was
  // only dropped on a sessionId-change effect.
  await expect(page.getByTestId('todo-panel')).toHaveCount(0, { timeout: 5_000 })

  // Backend regression: the `/todos` snapshot must be empty too, otherwise
  // re-opening the session would refetch and re-render the stale list.
  const todosRes = await request.get(`/api/sessions/${sessionId}/todos`, { headers: authHeader })
  expect(todosRes.ok()).toBeTruthy()
  expect((await todosRes.json()) as { todos: unknown[] }).toEqual({ todos: [] })

  // Reload also stays clean — confirms the server-side wipe took.
  await page.reload()
  await expect(page.locator('.chat-empty')).toBeVisible({ timeout: 10_000 })
  await expect(page.getByTestId('todo-panel')).toHaveCount(0)
})

test('clearing the session live-clears the dedicated session-todos view', async ({
  request,
  page,
  baseURL,
}) => {
  // The standalone SessionTodosView didn't have ChatView's events.length===0
  // fallback, so a clear used to leave it rendering the load-time snapshot
  // forever. The session-cleared broadcast now drops the cached snapshot
  // here too.
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedAuthedSession(request, authHeader)

  // Seed todos before opening the dedicated view so the load-time fetch
  // hydrates `loadedTodos` with the pre-clear list.
  const sendRes = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'track some work', model: 'mock:todo' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  await loadAppAt(page, token, `/sessions/${sessionId}/todos`)
  await expect(page.getByTestId('session-todos-view')).toBeVisible({ timeout: 10_000 })
  await expect(page.getByTestId('todo-panel')).toBeVisible({ timeout: 10_000 })
  await expect(page.getByTestId('todo-item')).toHaveCount(3)

  const clearRes = await request.post(`/api/sessions/${sessionId}/clear`, { headers: authHeader })
  expect(clearRes.ok(), `clear failed: ${await clearRes.text()}`).toBeTruthy()

  // Live: empty-state copy replaces the panel without a reload.
  await expect(page.getByTestId('session-todos-empty')).toBeVisible({ timeout: 5_000 })
  await expect(page.getByTestId('todo-panel')).toHaveCount(0)
})
