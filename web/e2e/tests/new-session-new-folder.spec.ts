import { test, expect, type APIRequestContext, type Page } from '../harness'
import { existsSync, mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * "+ New Folder" from the create dialogs.
 *
 * The New Session modal used to carry an inline name/path card that called
 * createFolder without the create-dir flag, so a path that didn't exist yet
 * always failed. It now opens the shared NewFolderModal stacked on top; the
 * created folder is auto-selected in the host form. The repeating-task
 * modal gets the same entry point in place of its "use the folder manager
 * first" dead end.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

async function loadApp(page: Page, token: string) {
  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto('/')
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })
}

/** A directory path under a fresh tmp dir that does not exist yet. */
function freshMissingDir(prefix: string) {
  const dir = path.join(mkdtempSync(path.join(tmpdir(), prefix)), 'not-yet-created')
  expect(existsSync(dir)).toBe(false)
  return dir
}

/** Fill and submit the stacked New Folder dialog with create-dir checked. */
async function createFolderInDialog(page: Page, name: string, dir: string) {
  const dialog = page.getByTestId('new-folder-modal')
  await expect(dialog).toBeVisible()
  await page.getByTestId('new-folder-name').fill(name)
  await page.getByTestId('new-folder-path').fill(dir)
  // Blur the path combobox so its suggestion list can't cover the checkbox.
  await page.getByTestId('new-folder-name').click()
  // The box follows the server's existence probe until touched — wait for
  // the verdict so check() doesn't race the auto-check.
  await expect(page.getByTestId('folder-path-status')).toContainText("doesn't exist")
  await page.getByTestId('new-folder-create-dir').check()
  await page.getByTestId('new-folder-submit').click()
  await expect(dialog).toHaveCount(0)
}

test('New Session → + New Folder creates a missing dir, selects it, and the session lands in it', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  await loadApp(page, token)

  await page.locator('.tab-new').click()
  const sessionModal = page.locator('.modal', { hasText: 'New Session' })
  await expect(page.getByTestId('new-session-preset')).toBeVisible()
  const sessionName = `new-folder-session-${Date.now()}`
  await page.locator('#new-session-name').fill(sessionName)

  // Escape in the stacked dialog closes only that dialog — the half-filled
  // New Session form stays up.
  await page.getByTestId('new-session-new-folder').click()
  await expect(page.getByTestId('new-folder-modal')).toBeVisible()
  await page.getByTestId('new-folder-name').press('Escape')
  await expect(page.getByTestId('new-folder-modal')).toHaveCount(0)
  await expect(sessionModal).toBeVisible()
  await expect(page.locator('#new-session-name')).toHaveValue(sessionName)

  const dir = freshMissingDir('peckboard-e2e-new-folder-')
  const folderName = `e2e-new-folder-${Date.now()}`
  await page.getByTestId('new-session-new-folder').click()
  await createFolderInDialog(page, folderName, dir)

  // Submitting the nested dialog must not have submitted the session form.
  await expect(sessionModal).toBeVisible()
  expect(existsSync(dir)).toBe(true)
  const select = page.locator('#new-session-folder')
  await expect(select.locator('option:checked')).toContainText(folderName)
  const folderId = await select.inputValue()

  await page.getByTestId('new-session-model').click()
  await page.getByTestId('new-session-model-search').fill('happy')
  await page.getByRole('option', { name: 'Mock: happy path' }).click()
  await page.getByRole('button', { name: 'Create Session' }).click()
  await expect(page.locator('.chat-toolbar-name')).toHaveText(sessionName, { timeout: 10_000 })

  await expect
    .poll(async () => {
      const res = await request.get('/api/sessions', { headers: authHeader })
      const { items } = (await res.json()) as {
        items: Array<{ name: string; folder_id: string }>
      }
      return items.find((s) => s.name === sessionName)?.folder_id ?? null
    })
    .toBe(folderId)
})

test('New Repeating Task → + New Folder auto-selects the created folder', async ({
  request,
  page,
}) => {
  const { token } = await authenticate(request)
  await loadApp(page, token)

  await page.locator('.rail-btn[title="Repeating Tasks"]').click()
  await page.getByRole('button', { name: /new task/i }).click()
  await expect(page.getByRole('heading', { name: /new repeating task/i })).toBeVisible()

  const dir = freshMissingDir('peckboard-e2e-rt-new-folder-')
  const folderName = `e2e-rt-new-folder-${Date.now()}`
  await page.getByTestId('repeating-task-new-folder').click()
  await createFolderInDialog(page, folderName, dir)

  await expect(page.getByRole('heading', { name: /new repeating task/i })).toBeVisible()
  await expect(page.locator('#repeating-task-folder option:checked')).toContainText(folderName)
})
