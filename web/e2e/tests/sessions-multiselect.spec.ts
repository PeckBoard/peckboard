import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E for multi-selecting sessions on the sessions list. The list intentionally
 * does NOT expose delete — that lives on the chat-toolbar 3-dot menu and the
 * tab right-click menu so the user has the session open and can act
 * intentionally. The bar surfaces non-destructive bulk actions only (currently
 * just "Mark as read", which is hidden unless any selected session is unread),
 * plus a Clear button that empties the selection.
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
): Promise<void> {
  const res = await request.post('/api/sessions', {
    headers: auth,
    data: { name, folder_id: folderId },
  })
  expect(res.ok(), `create session failed: ${await res.text()}`).toBeTruthy()
}

async function loadAs(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
  await expect(page.locator('.rail-avatar')).toBeVisible({ timeout: 10_000 })
}

test('multi-select surfaces a non-destructive bulk bar with no delete option', async ({
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
  await createSession(request, auth, folderId, nameA)
  await createSession(request, auth, folderId, nameB)
  await createSession(request, auth, folderId, nameC)

  await loadAs(page, token, '/')

  const rowA = page.locator('.list-view-row', { hasText: nameA })
  const rowB = page.locator('.list-view-row', { hasText: nameB })
  const rowC = page.locator('.list-view-row', { hasText: nameC })
  await expect(rowA).toBeVisible()
  await expect(rowB).toBeVisible()
  await expect(rowC).toBeVisible()

  // Check the left-edge boxes for A and B.
  await rowA.locator('.list-view-select').check()
  await rowB.locator('.list-view-select').check()

  // The bulk-action bar appears with the selected count.
  const bar = page.locator('.bulk-action-bar')
  await expect(bar).toContainText('2 selected')

  // The card-level requirement is that the sessions list NEVER offers delete
  // — neither as a bulk action nor as a row 3-dot menu item. Assert both.
  await expect(bar.locator('.bulk-action-btn.danger')).toHaveCount(0)
  await expect(bar.locator('.bulk-action-btn', { hasText: /delete/i })).toHaveCount(0)
  await expect(rowA.locator('.list-view-menu')).toHaveCount(0)

  // Clear empties the selection and dismisses the bar; rows survive.
  await bar.locator('.bulk-action-btn', { hasText: 'Clear' }).click()
  await expect(bar).toHaveCount(0)
  await expect(rowA).toBeVisible()
  await expect(rowB).toBeVisible()
  await expect(rowC).toBeVisible()
})
