import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * PATCH /api/folders/{id} rename, and the Folders management dialog's
 * Rename action that drives it. Covers the happy path and the
 * duplicate-name 409. The renamed folder is fed to every picker off the
 * same folders Zustand store, so no per-consumer refetch is needed.
 */

const ADMIN_USER = 'e2e-user'
const ADMIN_PASS = 'e2e-password-1234'

let cachedAuth: { token: string; auth: Record<string, string> } | null = null

async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  if (cachedAuth) return cachedAuth
  const res = await request.post('/api/auth/login', {
    data: { username: ADMIN_USER, password: ADMIN_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  cachedAuth = { token, auth: { Authorization: `Bearer ${token}` } }
  return cachedAuth
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

async function createFolder(
  request: APIRequestContext,
  auth: Record<string, string>,
  slug: string,
): Promise<{ id: string; name: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-${slug}-`))
  const name = `e2e-${slug}-${Date.now()}`
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  const folder = (await res.json()) as { id: string }
  return { id: folder.id, name }
}

test('renaming a folder updates the list and rejects a duplicate name', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const a = await createFolder(request, auth, 'rename-a')
  const b = await createFolder(request, auth, 'rename-b')

  await loadAt(page, token, '/folders')
  const renameBtn = page.locator(`[data-testid="folder-rename-${a.name}"]`)
  await expect(renameBtn).toBeVisible({ timeout: 10_000 })

  // Duplicate name (case-insensitive) is rejected inline, dialog stays open.
  await renameBtn.click()
  const modal = page.locator('[data-testid="rename-modal"]')
  await expect(modal).toBeVisible()
  await page.locator('[data-testid="rename-input"]').fill(b.name.toUpperCase())
  await page.locator('[data-testid="rename-submit"]').click()
  await expect(page.locator('[data-testid="rename-error"]')).toBeVisible()
  await expect(modal).toBeVisible()

  // A real rename succeeds, closes the dialog, and the new name shows up
  // live in the folders list.
  const newName = `${a.name}-renamed`
  await page.locator('[data-testid="rename-input"]').fill(newName)
  await page.locator('[data-testid="rename-submit"]').click()
  await expect(modal).toHaveCount(0)
  await expect(page.locator(`[data-testid="folder-rename-${newName}"]`)).toBeVisible()
  await expect(page.locator(`[data-testid="folder-rename-${a.name}"]`)).toHaveCount(0)
})
