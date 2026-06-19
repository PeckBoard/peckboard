import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e test for the session-page dedicated todos/tasks view.
 *
 * Drives the real React app: logs in via the API, injects the token into
 * localStorage so the app boots authenticated, opens a session, triggers the
 * `mock:todo` scenario, and asserts the chat toolbar's Tasks button opens the
 * dedicated /sessions/{id}/todos route, where the same shared TodoPanel
 * renders the grouped snapshot. Also asserts the empty-state copy on a
 * session that has never reported any todos.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type AuthBundle = {
  token: string
  authHeader: { Authorization: string }
}

async function authenticate(request: APIRequestContext): Promise<AuthBundle> {
  const status = await request.get('/api/auth/status')
  expect(status.ok()).toBeTruthy()
  const { has_users } = (await status.json()) as { has_users: boolean }

  const credentials = { username: E2E_USER, password: E2E_PASS }
  const endpoint = has_users ? '/api/auth/login' : '/api/auth/register'

  const res = await request.post(endpoint, { data: credentials })
  expect(res.ok(), `auth via ${endpoint} failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

async function createFolderAndSession(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  label: string,
): Promise<{ sessionId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-${label}-`))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-${label}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: `${label} session`, folder_id: folder.id },
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

test('session todos button opens the dedicated view with the grouped snapshot', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await createFolderAndSession(request, authHeader, 'session-todos')

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  // Wait for the chat surface to render before kicking off the agent.
  await expect(page.locator('.chat-empty').or(page.locator('.chat-bubble').first())).toBeVisible({
    timeout: 10_000,
  })

  // Trigger the scripted todo snapshot (done: 1, in_progress: 1, pending: 1).
  const sendRes = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'track some work', model: 'mock:todo' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  // The chat-toolbar Tasks button shows the rolled-up done/total once todos arrive.
  const tasksBtn = page.getByTestId('chat-toolbar-tasks')
  await expect(tasksBtn).toBeVisible({ timeout: 10_000 })
  await expect(tasksBtn).toContainText('1/3')

  // Click to navigate to the dedicated view.
  await tasksBtn.click()
  await expect(page).toHaveURL(new RegExp(`/sessions/${sessionId}/todos$`))
  await expect(page.getByTestId('session-todos-view')).toBeVisible()

  // The shared TodoPanel renders the same grouped snapshot the chat view shows.
  await expect(page.getByTestId('todo-panel')).toBeVisible()
  await expect(page.getByTestId('todo-panel-count')).toHaveText('1/3 done')
  await expect(page.locator('[data-testid="todo-item"][data-status="done"]')).toContainText(
    'Write the parser',
  )
  await expect(page.locator('[data-testid="todo-item"][data-status="in_progress"]')).toContainText(
    'Wiring up the route',
  )
  await expect(page.locator('[data-testid="todo-item"][data-status="pending"]')).toContainText(
    'Add tests',
  )

  // Back affordance returns to the chat view at /sessions/{id}.
  await page.getByTestId('session-todos-back').click()
  await expect(page).toHaveURL(new RegExp(`/sessions/${sessionId}$`))
  await expect(page.getByTestId('session-todos-view')).toHaveCount(0)
})

test('session todos view shows an empty state for a session with no todos', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await createFolderAndSession(request, authHeader, 'session-todos-empty')

  // Deep-link directly into the dedicated view — no `todo` events have ever
  // landed for this session.
  await loadAppAt(page, token, `/sessions/${sessionId}/todos`)

  await expect(page.getByTestId('session-todos-view')).toBeVisible({ timeout: 10_000 })
  await expect(page.getByTestId('session-todos-empty')).toBeVisible()
  await expect(page.getByTestId('session-todos-empty')).toContainText('No tasks yet')
  await expect(page.getByTestId('todo-panel')).toHaveCount(0)
})
