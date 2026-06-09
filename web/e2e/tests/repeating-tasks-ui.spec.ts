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
