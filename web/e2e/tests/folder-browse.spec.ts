import { test, expect, type APIRequestContext, type Page } from '../harness'
import { existsSync, mkdirSync, mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * The folder-registration path field is a server-backed autocomplete:
 * typing an absolute path suggests matching subdirectories (from
 * `GET /api/folders/browse`) and a status line reports whether the typed
 * directory exists, so the "Create directory" checkbox is an informed
 * choice. Plain typing must keep working — the popup only assists.
 */

const ADMIN_USER = 'e2e-user'
const ADMIN_PASS = 'e2e-password-1234'

let cachedAuth: { token: string; auth: Record<string, string> } | null = null

/** Authenticate as the bootstrap admin once per spec file — the per-IP
 *  login limiter sees the whole suite as one client. */
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

/** Plant a token in localStorage, load the Folders page, and open the
 *  New Folder dialog. */
async function openNewFolder(page: Page, token: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto('/folders')
  await page.getByTestId('folders-new-folder').click()
  await expect(page.getByTestId('new-folder-modal')).toBeVisible()
}

test('typing a path suggests subdirectories and picking one validates + registers it', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const root = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-browse-'))
  for (const sub of ['apples', 'apricots', 'bananas']) {
    mkdirSync(path.join(root, sub))
  }

  await openNewFolder(page, token)
  const input = page.getByTestId('new-folder-path')
  await expect(input).toBeVisible({ timeout: 10_000 })

  // A partial final segment prefix-filters the suggestions.
  await input.fill(`${root}/ap`)
  await expect(page.locator('[data-testid="path-suggestion-apples"]')).toBeVisible()
  await expect(page.locator('[data-testid="path-suggestion-apricots"]')).toBeVisible()
  await expect(page.locator('[data-testid="path-suggestion-bananas"]')).toHaveCount(0)

  // The partial path doesn't exist yet, and the status line says so.
  const status = page.locator('[data-testid="folder-path-status"]')
  await expect(status).toContainText("doesn't exist")

  // Picking a suggestion fills the field and flips the status to exists.
  await page.locator('[data-testid="path-suggestion-apples"]').click()
  await expect(input).toHaveValue(path.join(root, 'apples'))
  await expect(status).toContainText('exists on the server')

  // The informed create goes through without the create-directory checkbox.
  const name = `e2e-browse-${Date.now()}`
  await page.getByTestId('new-folder-name').fill(name)
  await page.getByTestId('new-folder-submit').click()
  await expect(page.getByTestId(`folder-row-${name}`)).toBeVisible()

  const list = await request.get('/api/folders', { headers: auth })
  expect(list.ok()).toBeTruthy()
  const folders = (await list.json()) as { name: string; path: string }[]
  expect(folders.find((f) => f.name === name)?.path).toBe(path.join(root, 'apples'))
})

test('a nonexistent path is flagged, and create-directory defaults on until toggled', async ({
  request,
  page,
}) => {
  const { token } = await authenticate(request)
  const root = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-browse2-'))
  const target = path.join(root, 'brand-new-dir')

  await openNewFolder(page, token)
  const input = page.getByTestId('new-folder-path')
  await expect(input).toBeVisible({ timeout: 10_000 })

  // Typed path doesn't exist: create-directory is pre-checked and the
  // status says the directory will be created.
  await input.fill(target)
  const status = page.locator('[data-testid="folder-path-status"]')
  const createDir = page.getByTestId('new-folder-create-dir')
  await expect(status).toContainText('will be created')
  await expect(createDir).toBeChecked()

  // Unticking turns it back into a warning pointing at the checkbox.
  await createDir.uncheck()
  await expect(status).toContainText("doesn't exist")
  await expect(status).toContainText('Create directory')
  await createDir.check()
  await expect(status).toContainText('will be created')

  const name = `e2e-browse-new-${Date.now()}`
  await page.getByTestId('new-folder-name').fill(name)
  await page.getByTestId('new-folder-submit').click()
  await expect(page.getByTestId(`folder-row-${name}`)).toBeVisible()
  expect(existsSync(target)).toBe(true)
})
