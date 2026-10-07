import { test, expect, type APIRequestContext, type Page } from '../harness'
import { existsSync, mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Folders page → "New Folder" dialog (`NewFolderModal`): registering a
 * directory that doesn't exist yet (create-directory defaults on), and the
 * inline validation that keeps the submit disabled with a stated reason.
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

async function openNewFolder(page: Page, token: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto('/folders')
  await expect(page.getByRole('heading', { name: 'Folders', exact: true })).toBeVisible({
    timeout: 10_000,
  })
  await page.getByTestId('folders-new-folder').click()
  const modal = page.getByTestId('new-folder-modal')
  await expect(modal).toBeVisible()
  await expect(modal.getByRole('heading', { name: 'New Folder' })).toBeVisible()
  return modal
}

test('an admin creates a folder for a new directory from the Folders page', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const target = path.join(mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-newfolder-')), 'fresh')
  const name = `e2e-new-folder-${Date.now()}`

  const modal = await openNewFolder(page, token)
  await modal.getByTestId('new-folder-name').fill(name)
  await modal.getByTestId('new-folder-path').fill(target)
  await expect(modal.getByTestId('folder-path-status')).toContainText('will be created')
  await expect(modal.getByTestId('new-folder-create-dir')).toBeChecked()

  await modal.getByTestId('new-folder-submit').click()
  await expect(modal).toBeHidden({ timeout: 10_000 })
  await expect(page.getByTestId(`folder-row-${name}`)).toBeVisible()
  await expect(page.getByTestId(`folder-row-${name}`)).toContainText(target)
  expect(existsSync(target)).toBe(true)

  const list = await request.get('/api/folders', { headers: auth })
  const folders = (await list.json()) as { name: string; path: string }[]
  expect(folders.find((f) => f.name === name)?.path).toBe(target)
})

test('a relative path is flagged inline and keeps the submit disabled', async ({
  request,
  page,
}) => {
  const { token } = await authenticate(request)
  const modal = await openNewFolder(page, token)
  const submit = modal.getByTestId('new-folder-submit')

  // Empty form: disabled, and the reason names what's missing.
  await expect(submit).toBeDisabled()
  await expect(modal.getByTestId('new-folder-disabled-reason')).toHaveText('Enter a name')

  await modal.getByTestId('new-folder-name').fill('e2e relative')
  await modal.getByTestId('new-folder-path').fill('relative/dir')
  await expect(modal.getByTestId('new-folder-path-error')).toContainText('absolute')
  await expect(modal.getByTestId('new-folder-disabled-reason')).toHaveText('Path must be absolute')
  await expect(submit).toBeDisabled()

  // Escape closes the dialog without touching the page underneath.
  await page.keyboard.press('Escape')
  await expect(modal).toBeHidden()
  await expect(page.getByRole('heading', { name: 'Folders', exact: true })).toBeVisible()
})
