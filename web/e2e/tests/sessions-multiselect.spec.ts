import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E for multi-selecting sessions on the sessions list: the floating bulk
 * bar shows the count, "Mark as read" (hidden unless a selected session is
 * unread), a danger "Delete" that always goes through a confirm, and Clear.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function login(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { token: string }).token
}

async function createFolder(
  request: APIRequestContext,
  auth: Record<string, string>,
): Promise<string> {
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'ms-folder', path: mkdtempSync(path.join(tmpdir(), 'pb-ms-')) },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function createSession(
  request: APIRequestContext,
  auth: Record<string, string>,
  folderId: string,
  name: string,
): Promise<string> {
  const res = await request.post('/api/sessions', {
    headers: auth,
    data: { name, folder_id: folderId },
  })
  expect(res.ok(), `create session failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function loadAs(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
  await expect(page.locator('.rail-avatar')).toBeVisible({ timeout: 10_000 })
}

test('bulk delete removes the selected sessions after a confirm; Clear keeps them', async ({
  request,
  page,
}) => {
  const token = await login(request)
  const auth = { Authorization: `Bearer ${token}` }
  const folderId = await createFolder(request, auth)

  const ts = Date.now()
  const nameA = `ms-A-${ts}`
  const nameB = `ms-B-${ts}`
  const nameC = `ms-C-${ts}`
  const idA = await createSession(request, auth, folderId, nameA)
  const idB = await createSession(request, auth, folderId, nameB)
  const idC = await createSession(request, auth, folderId, nameC)

  await loadAs(page, token, '/')

  const rowA = page.locator('.list-view-row', { hasText: nameA })
  const rowB = page.locator('.list-view-row', { hasText: nameB })
  const rowC = page.locator('.list-view-row', { hasText: nameC })
  await expect(rowA).toBeVisible()
  await expect(rowB).toBeVisible()
  await expect(rowC).toBeVisible()

  // Clear empties the selection and dismisses the bar; rows survive.
  await rowC.locator('.list-view-select').check()
  const bar = page.locator('.bulk-action-bar')
  await expect(bar).toContainText('1 selected')
  await bar.locator('.bulk-action-btn', { hasText: 'Clear' }).click()
  await expect(bar).toHaveCount(0)
  await expect(rowC).toBeVisible()

  await rowA.locator('.list-view-select').check()
  await rowB.locator('.list-view-select').check()
  await expect(bar).toContainText('2 selected')

  // Delete asks first; Cancel deletes nothing and keeps the selection.
  const del = bar.locator('.bulk-action-btn.danger', { hasText: 'Delete' })
  await del.click()
  const dialog = page.getByRole('alertdialog', { name: 'Delete 2 sessions' })
  await expect(dialog).toBeVisible()
  await expect(dialog).toContainText('these 2 sessions')
  await dialog.getByTestId('confirm-dialog-cancel').click()
  await expect(dialog).toHaveCount(0)
  await expect(rowA).toBeVisible()
  await expect(bar).toContainText('2 selected')

  // Confirming deletes both, clears the selection, and leaves C alone.
  await del.click()
  await dialog.getByRole('button', { name: 'Delete', exact: true }).click()
  await expect(dialog).toHaveCount(0)
  await expect(rowA).toHaveCount(0)
  await expect(rowB).toHaveCount(0)
  await expect(bar).toHaveCount(0)
  await expect(rowC).toBeVisible()

  for (const id of [idA, idB]) {
    const res = await request.get(`/api/sessions/${id}`, { headers: auth })
    expect(res.status(), `session ${id} should be gone`).toBe(404)
  }
  const kept = await request.get(`/api/sessions/${idC}`, { headers: auth })
  expect(kept.ok()).toBeTruthy()
})
