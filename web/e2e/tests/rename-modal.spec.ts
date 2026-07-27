import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E for the shared rename dialog (`RenameModal`), which replaced the
 * four `window.prompt` call sites.
 *
 * Covered here:
 *   - chat toolbar → Rename renames the session
 *   - empty / whitespace-only input is rejected inline, nothing is sent
 *   - a refused rename shows the server's message and keeps the dialog
 *     open so the same input can be retried
 *   - project tab → Rename and repeating-task tab → Rename
 *
 * The fourth call site (session tab → Rename) is covered by
 * `tabs.spec.ts`, which already owns the tab-strip flows.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

let cachedAuth: { token: string; authHeader: Record<string, string> } | null = null

/** Authenticate once per spec file — the rate limiter sees every request
 *  from 127.0.0.1 as one client. */
async function authenticate(request: APIRequestContext) {
  if (cachedAuth) return cachedAuth
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  cachedAuth = { token, authHeader: { Authorization: `Bearer ${token}` } }
  return cachedAuth
}

async function seedFolder(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  label: string,
): Promise<string> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-rename-${label}-`))
  const res = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-rename-${label}`, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function seedSession(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  name: string,
): Promise<string> {
  const folderId = await seedFolder(request, authHeader, name)
  const res = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name, folder_id: folderId },
  })
  expect(res.ok(), `create session failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })
}
/** Clear the server-side tab list so a test starts from a clean strip —
 *  every test here opens tabs, and stale ones make label locators
 *  ambiguous on retry. */
async function clearTabs(request: APIRequestContext, authHeader: Record<string, string>) {
  const res = await request.get('/api/me/tabs', { headers: authHeader })
  if (!res.ok()) return
  const tabs = (await res.json()) as Array<{ item_type: string; item_id: string }>
  for (const t of tabs) {
    await request.delete(`/api/me/tabs/${t.item_type}/${t.item_id}`, { headers: authHeader })
  }
}

/** The tab strip entry for one item — keyed by id, so a rename to a name
 *  another test also used stays unambiguous. */
function tab(page: Page, key: string) {
  return page.locator(`[data-tab-key="${key}"]`)
}

/** Right-click a tab and pick its Rename item. */
async function renameFromTabMenu(page: Page, key: string) {
  const target = tab(page, key)
  await expect(target).toBeVisible()
  await target.click({ button: 'right' })
  const renameBtn = page.locator('.context-menu button', { hasText: 'Rename' })
  await expect(renameBtn).toBeVisible()
  await renameBtn.click()
}

async function sessionName(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  sessionId: string,
): Promise<string> {
  const res = await request.get(`/api/sessions/${sessionId}`, { headers: authHeader })
  expect(res.ok()).toBeTruthy()
  return ((await res.json()) as { name: string }).name
}

test('chat toolbar Rename opens the dialog and renames the session', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  await clearTabs(request, authHeader)
  const sessionId = await seedSession(request, authHeader, 'toolbar-before')

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  await page.locator('.chat-toolbar-menu').click()
  await page.getByTestId('chat-menu-rename').click()

  // Prefilled from the session, and Enter submits.
  const input = page.getByTestId('rename-input')
  await expect(input).toBeFocused()
  await expect(input).toHaveValue('toolbar-before')
  await input.fill('toolbar-after')
  await input.press('Enter')

  await expect(page.getByTestId('rename-modal')).toHaveCount(0)
  await expect(tab(page, `session:${sessionId}`)).toContainText('toolbar-after')
  expect(await sessionName(request, authHeader, sessionId)).toBe('toolbar-after')
})

test('an empty or whitespace-only name is rejected and never reaches the server', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  await clearTabs(request, authHeader)
  const sessionId = await seedSession(request, authHeader, 'keep-my-name')

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  let patches = 0
  await page.route(`**/api/sessions/${sessionId}`, async (route) => {
    if (route.request().method() === 'PATCH') patches++
    await route.continue()
  })

  await page.locator('.chat-toolbar-menu').click()
  await page.getByTestId('chat-menu-rename').click()

  const input = page.getByTestId('rename-input')
  await input.fill('')
  await page.getByTestId('rename-submit').click()
  await expect(page.getByTestId('rename-error')).toHaveText('Name cannot be empty')
  await expect(page.getByTestId('rename-modal')).toBeVisible()

  // Whitespace-only is the same rejection; typing clears the error.
  await input.fill('   ')
  await expect(page.getByTestId('rename-error')).toHaveCount(0)
  await input.press('Enter')
  await expect(page.getByTestId('rename-error')).toHaveText('Name cannot be empty')
  await expect(page.getByTestId('rename-modal')).toBeVisible()

  expect(patches).toBe(0)
  expect(await sessionName(request, authHeader, sessionId)).toBe('keep-my-name')

  // Escape cancels without touching the name.
  await page.keyboard.press('Escape')
  await expect(page.getByTestId('rename-modal')).toHaveCount(0)
  expect(await sessionName(request, authHeader, sessionId)).toBe('keep-my-name')
})

test('a refused rename shows the server error and keeps the dialog open', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  await clearTabs(request, authHeader)
  const sessionId = await seedSession(request, authHeader, 'refused-before')

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  const pattern = `**/api/sessions/${sessionId}`
  await page.route(pattern, async (route) => {
    if (route.request().method() !== 'PATCH') return route.continue()
    await route.fulfill({
      status: 500,
      contentType: 'application/json',
      body: JSON.stringify({ error: 'Rename refused: that name is taken.' }),
    })
  })

  await page.locator('.chat-toolbar-menu').click()
  await page.getByTestId('chat-menu-rename').click()

  const input = page.getByTestId('rename-input')
  await input.fill('refused-after')
  await page.getByTestId('rename-submit').click()

  await expect(page.getByTestId('rename-error')).toHaveText('Rename refused: that name is taken.')
  await expect(page.getByTestId('rename-modal')).toBeVisible()
  await expect(input).toHaveValue('refused-after')
  expect(await sessionName(request, authHeader, sessionId)).toBe('refused-before')

  // The same click retries once the server is healthy again.
  await page.unroute(pattern)
  await page.getByTestId('rename-submit').click()
  await expect(page.getByTestId('rename-modal')).toHaveCount(0)
  expect(await sessionName(request, authHeader, sessionId)).toBe('refused-after')
})

test('project tab Rename renames the project', async ({ request, page, baseURL }) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  await clearTabs(request, authHeader)
  const folderId = await seedFolder(request, authHeader, 'proj')
  const projectRes = await request.post('/api/projects', {
    headers: authHeader,
    data: {
      name: 'proj-before',
      folder_id: folderId,
      worker_count: 1,
      model: 'mock:happy-path',
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const projectId = ((await projectRes.json()) as { id: string }).id

  await loadAppAt(page, token, `/projects/${projectId}`)
  await renameFromTabMenu(page, `project:${projectId}`)

  const input = page.getByTestId('rename-input')
  await expect(input).toHaveValue('proj-before')
  await input.fill('proj-after')
  await page.getByTestId('rename-submit').click()

  await expect(page.getByTestId('rename-modal')).toHaveCount(0)
  await expect(tab(page, `project:${projectId}`)).toContainText('proj-after')
  const check = await request.get(`/api/projects/${projectId}`, { headers: authHeader })
  expect(check.ok()).toBeTruthy()
  expect(((await check.json()) as { project: { name: string } }).project.name).toBe('proj-after')
})

test('repeating task tab Rename renames the task', async ({ request, page, baseURL }) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  await clearTabs(request, authHeader)
  const folderId = await seedFolder(request, authHeader, 'task')
  const taskRes = await request.post('/api/repeating-tasks', {
    headers: authHeader,
    data: {
      name: 'task-before',
      folder_id: folderId,
      prompt: 'go',
      schedule_kind: 'interval',
      schedule_value: { minutes: 60 },
      model: 'mock:happy-path',
      enabled: false,
    },
  })
  expect(taskRes.ok(), `create task failed: ${await taskRes.text()}`).toBeTruthy()
  const taskId = ((await taskRes.json()) as { id: string }).id

  await loadAppAt(page, token, `/repeating-tasks/${taskId}`)
  await renameFromTabMenu(page, `repeating_task:${taskId}`)

  const input = page.getByTestId('rename-input')
  await expect(input).toHaveValue('task-before')
  await input.fill('task-after')
  await page.getByTestId('rename-submit').click()

  await expect(page.getByTestId('rename-modal')).toHaveCount(0)
  await expect(tab(page, `repeating_task:${taskId}`)).toContainText('task-after')
  const check = await request.get(`/api/repeating-tasks/${taskId}`, { headers: authHeader })
  expect(check.ok()).toBeTruthy()
  expect(((await check.json()) as { name: string }).name).toBe('task-after')
})
