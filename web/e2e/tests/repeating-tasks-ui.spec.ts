import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI-level coverage for the Repeating Tasks nav entry and list view.
 *
 * Exercises:
 *  - The rail button between Sessions and Projects exists and routes to
 *    `/repeating-tasks`.
 *  - The list view loads server-side data.
 *  - Clicking "Run now" surfaces a status banner.
 *  - The pause/resume action flips the badge.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok()).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

async function loginUi(page: import('@playwright/test').Page, baseURL: string) {
  await page.goto(baseURL)
  await page.getByLabel('Username').fill(E2E_USER)
  await page.getByLabel('Password').fill(E2E_PASS)
  await page.getByRole('button', { name: /sign in/i }).click()
}

test('rail nav opens the Repeating Tasks list view', async ({ page, baseURL, request }) => {
  expect(baseURL).toBeTruthy()
  const token = await authenticate(request)

  // Seed a folder + task so the list isn't empty.
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-ui-rt-'))
  const folderRes = await request.post('/api/folders', {
    headers: { Authorization: `Bearer ${token}` },
    data: { name: 'e2e-ui-rt', path: folderPath },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const taskRes = await request.post('/api/repeating-tasks', {
    headers: { Authorization: `Bearer ${token}` },
    data: {
      name: 'ui smoke task',
      description: 'used by the ui test',
      folder_id: folder.id,
      prompt: 'go',
      schedule_kind: 'interval',
      schedule_value: { minutes: 15 },
    },
  })
  expect(taskRes.ok()).toBeTruthy()

  await loginUi(page, baseURL!)
  // Wait for the rail to render (login flow).
  await expect(page.locator('.rail')).toBeVisible()

  await page.locator('.rail-btn[title="Repeating Tasks"]').click()
  await expect(page).toHaveURL(/\/repeating-tasks$/)
  await expect(page.getByRole('heading', { name: 'Repeating Tasks' })).toBeVisible()
  await expect(page.getByText('ui smoke task')).toBeVisible()
})

test('model + effort are settable from the create form and used for the spawned run', async ({
  page,
  baseURL,
  request,
}) => {
  expect(baseURL).toBeTruthy()
  const token = await authenticate(request)
  const authHeader = { Authorization: `Bearer ${token}` }

  // Seed a folder so the form's folder picker has a target.
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-ui-rt-model-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-ui-rt-model', path: folderPath },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  await loginUi(page, baseURL!)
  await expect(page.locator('.rail')).toBeVisible()
  await page.locator('.rail-btn[title="Repeating Tasks"]').click()
  await expect(page.getByRole('heading', { name: 'Repeating Tasks' })).toBeVisible()

  await page.getByRole('button', { name: /new task/i }).click()
  await expect(page.getByRole('heading', { name: /new repeating task/i })).toBeVisible()

  const taskName = `model-effort-task-${Date.now()}`
  await page.getByPlaceholder(/daily project sweep/i).fill(taskName)
  // The folder picker is the first .form-input select; pick the seeded
  // folder by id rather than label so we don't race against other folders
  // that earlier tests may have left in the list.
  await page.locator('select.form-input').first().selectOption({ value: folder.id })
  await page.getByPlaceholder(/message sent to the new session/i).fill('do thing')
  // Model select picks the mock provider so the run is deterministic.
  await page.getByLabel('Model').selectOption({ value: 'mock:happy-path' })
  await page.getByLabel('Effort').selectOption({ value: 'high' })
  await page.getByRole('button', { name: /create task/i }).click()

  // Detail view opens automatically; verify the metadata reflects what we set.
  await expect(page.getByRole('heading', { name: taskName })).toBeVisible()
  await expect(page.getByText('mock:happy-path')).toBeVisible()
  await expect(page.getByText('high', { exact: true })).toBeVisible()

  // Force-run from the UI. The spawned session must carry the task's model
  // so the run actually starts with it — that's the load-bearing claim.
  await page.getByRole('button', { name: /run now/i }).click()
  await expect(page.getByText(/spawned a new session/i)).toBeVisible()

  // Locate the task id via the API, then inspect the session it spawned.
  const listRes = await request.get('/api/repeating-tasks', { headers: authHeader })
  const tasks = (await listRes.json()) as Array<{ id: string; name: string }>
  const created = tasks.find((t) => t.name === taskName)
  expect(created, 'created task missing from list').toBeTruthy()
  const sessionsRes = await request.get(`/api/repeating-tasks/${created!.id}/sessions`, {
    headers: authHeader,
  })
  const sessions = (await sessionsRes.json()) as Array<{
    id: string
    model: string | null
    effort: string | null
  }>
  expect(sessions.length).toBeGreaterThan(0)
  expect(sessions[0].model).toBe('mock:happy-path')
  expect(sessions[0].effort).toBe('high')
})

test('editing preserves model/effort and round-trips changes', async ({
  page,
  baseURL,
  request,
}) => {
  expect(baseURL).toBeTruthy()
  const token = await authenticate(request)
  const authHeader = { Authorization: `Bearer ${token}` }

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-ui-rt-edit-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-ui-rt-edit', path: folderPath },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  // Seed the task via API with a model already set so we can confirm the
  // edit modal pre-fills it and the round-trip preserves a different value.
  const taskName = `edit-model-${Date.now()}`
  const taskRes = await request.post('/api/repeating-tasks', {
    headers: authHeader,
    data: {
      name: taskName,
      folder_id: folder.id,
      prompt: 'go',
      schedule_kind: 'interval',
      schedule_value: { minutes: 60 },
      model: 'mock:happy-path',
      effort: 'low',
    },
  })
  expect(taskRes.ok()).toBeTruthy()
  const task = (await taskRes.json()) as { id: string }

  await loginUi(page, baseURL!)
  await expect(page.locator('.rail')).toBeVisible()
  await page.locator('.rail-btn[title="Repeating Tasks"]').click()
  await expect(page.getByRole('heading', { name: 'Repeating Tasks' })).toBeVisible()
  await page.getByText(taskName).click()
  await expect(page.getByRole('heading', { name: taskName })).toBeVisible()
  // Detail panel shows the values that were created via API.
  await expect(page.getByText('mock:happy-path')).toBeVisible()
  await expect(page.getByText('low', { exact: true })).toBeVisible()

  // Open edit modal. The Model select should be pre-populated.
  await page.getByRole('button', { name: /^edit$/i }).click()
  await expect(page.getByRole('heading', { name: /edit repeating task/i })).toBeVisible()
  await expect(page.getByLabel('Model')).toHaveValue('mock:happy-path')
  await expect(page.getByLabel('Effort')).toHaveValue('low')

  // Change effort and save.
  await page.getByLabel('Effort').selectOption({ value: 'max' })
  await page.getByRole('button', { name: /^save$/i }).click()
  // Detail panel should reflect the new effort.
  await expect(page.getByText('max', { exact: true })).toBeVisible()

  // Confirm via API too — the PATCH path persisted the change.
  const getRes = await request.get(`/api/repeating-tasks/${task.id}`, { headers: authHeader })
  const persisted = (await getRes.json()) as { model: string | null; effort: string | null }
  expect(persisted.model).toBe('mock:happy-path')
  expect(persisted.effort).toBe('max')
})

test('pause flips visible badge between active and paused', async ({ page, baseURL, request }) => {
  expect(baseURL).toBeTruthy()
  const token = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-ui-rt-pause-'))
  const folderRes = await request.post('/api/folders', {
    headers: { Authorization: `Bearer ${token}` },
    data: { name: 'e2e-ui-rt-pause', path: folderPath },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const taskRes = await request.post('/api/repeating-tasks', {
    headers: { Authorization: `Bearer ${token}` },
    data: {
      name: 'pause-target',
      folder_id: folder.id,
      prompt: 'go',
      schedule_kind: 'interval',
      schedule_value: { minutes: 30 },
    },
  })
  expect(taskRes.ok()).toBeTruthy()

  // Log in, click into the Repeating Tasks list, then the row. Avoids a
  // hard navigation which forces a fresh checkAuth round-trip and races
  // the fetchTasks call.
  await loginUi(page, baseURL!)
  await expect(page.locator('.rail')).toBeVisible()
  await page.locator('.rail-btn[title="Repeating Tasks"]').click()
  await expect(page.getByRole('heading', { name: 'Repeating Tasks' })).toBeVisible()
  await page.getByText('pause-target').click()
  await expect(page.getByRole('heading', { name: 'pause-target' })).toBeVisible()

  // Pause via header action; "Pause" label should swap to "Resume".
  await page.getByRole('button', { name: /^pause$/i }).click()
  await expect(page.getByRole('button', { name: /^resume$/i })).toBeVisible()
  await expect(page.getByText('— (disabled)')).toBeVisible()

  // Resume.
  await page.getByRole('button', { name: /^resume$/i }).click()
  await expect(page.getByRole('button', { name: /^pause$/i })).toBeVisible()
})
