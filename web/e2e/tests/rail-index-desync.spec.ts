import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Regression test: the rail's Reports and Repeating Tasks buttons must
 * clear their view's active-detail id before navigating, mirroring the
 * Sessions/Projects/Review buttons. Without that, clicking the rail
 * button while a detail is open leaves the URL desynced from the index
 * view (or bounces straight back into the old detail).
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok()).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
  await expect(page.locator('.rail')).toBeVisible({ timeout: 10_000 })
}

async function seedFolder(
  request: APIRequestContext,
  auth: Record<string, string>,
  prefix: string,
): Promise<string> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-${prefix}-`))
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-${prefix}`, path: folderPath },
  })
  expect(res.ok()).toBeTruthy()
  const folder = (await res.json()) as { id: string }
  return folder.id
}

test('rail Repeating Tasks button clears active task and shows the index', async ({
  page,
  baseURL,
  request,
}) => {
  expect(baseURL).toBeTruthy()
  const { token, auth } = await authenticate(request)
  const folderId = await seedFolder(request, auth, 'rail-desync-rt')
  const taskRes = await request.post('/api/repeating-tasks', {
    headers: auth,
    data: {
      name: 'rail-desync task',
      description: '',
      folder_id: folderId,
      prompt: 'go',
      schedule_kind: 'interval',
      schedule_value: { minutes: 15 },
    },
  })
  expect(taskRes.ok()).toBeTruthy()
  const task = (await taskRes.json()) as { id: string }

  await loadAt(page, token, `/repeating-tasks/${task.id}`)
  await expect(page).toHaveURL(new RegExp(`/repeating-tasks/${task.id}$`))

  await page.locator('.rail-btn[title="Repeating Tasks"]').click()

  await expect(page).toHaveURL(/\/repeating-tasks$/)
  await expect(page.getByRole('heading', { name: 'Repeating Tasks' })).toBeVisible()
})

test('rail Reports button clears active report and shows the index', async ({
  page,
  baseURL,
  request,
}) => {
  expect(baseURL).toBeTruthy()
  const { token } = await authenticate(request)

  const dataDir = process.env.PECKBOARD_E2E_DATA_DIR
  if (!dataDir) throw new Error('PECKBOARD_E2E_DATA_DIR must be set (see playwright.config.ts)')
  const folder = 'rail-desync-reports'
  const file = 'note.md'
  const dir = path.join(dataDir, 'reports', folder)
  mkdirSync(dir, { recursive: true })
  writeFileSync(path.join(dir, file), '---\ntitle: rail desync note\n---\n\nbody\n')

  await loadAt(page, token, `/reports/${folder}/${file}`)
  await expect(page).toHaveURL(new RegExp(`/reports/${folder}/${file}$`))

  await page.locator('.rail-btn[title="Reports"]').click()

  await expect(page).toHaveURL(/\/reports$/)
  await expect(page.getByRole('heading', { name: 'Reports' })).toBeVisible()
})
