import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Polish batch: small chat / reports / settings fixes that each removed a
 * dead end from the UI.
 *
 *  1. an errored tool card opens itself instead of hiding the failure behind
 *     a collapsed row + red "Error" badge
 *  2. the "Uploading..." chip shows for the FIRST attachment (it used to be
 *     nested inside the `attachments.length > 0` guard, so the only feedback
 *     was the attach button greying out)
 *  3. + 4. accessible names on the icon-only attach / remove / delete-folder
 *     buttons
 *  5. a stale plugin-page URL explains itself and offers a way back instead
 *     of rendering a blank pane
 *  6. a failed report download says so instead of silently doing nothing
 *  8. the pre-hatcher model is the searchable ModelPicker, not a plain
 *     `<select>` over every provider × model
 *
 * Deterministic throughout: `mock:*` models drive the chat, and the failure
 * cases are routed at the network layer.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const PNG_1X1 = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==',
  'base64',
)

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function seedSession(
  request: APIRequestContext,
  auth: Record<string, string>,
  suffix: string,
  model = 'mock:happy-path',
): Promise<string> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-polish-${suffix}-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-polish-${suffix}-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: `polish ${suffix}`, folder_id: folder.id, model },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  return ((await sessionRes.json()) as { id: string }).id
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

/** Write a markdown report straight into the server's `reports/<folder>`
 *  directory — the only HTTP write endpoint is a PUT that needs the file
 *  to already exist (same trick as report-deeplink.spec.ts). */
function writeReportFile(folder: string, file: string, title: string) {
  const dataDir = process.env.PECKBOARD_E2E_DATA_DIR
  if (!dataDir) {
    throw new Error('PECKBOARD_E2E_DATA_DIR must be set (see playwright.config.ts)')
  }
  const dir = path.join(dataDir, 'reports', folder)
  mkdirSync(dir, { recursive: true })
  writeFileSync(
    path.join(dir, file),
    `---\ntitle: ${title}\ndate: ${folder}\n---\n\n# ${title}\n\nbody\n`,
  )
}

test('an errored tool card renders expanded, with the failure readable', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  // The session's own model wins over the one on the message POST, so the
  // scenario has to be pinned at creation time.
  const sessionId = await seedSession(request, auth, 'toolerr', 'mock:tool-error')

  await loadAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty')).toBeVisible({ timeout: 10_000 })

  const sendRes = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth,
    data: { text: 'go', model: 'mock:tool-error' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  const toolBlock = page.locator('.tool-block.tool-error').first()
  await expect(toolBlock).toBeVisible({ timeout: 10_000 })
  // Open by default: no click, the error text is already on screen.
  await expect(toolBlock.locator('.tool-header')).toHaveAttribute('aria-expanded', 'true')
  await expect(toolBlock.locator('.tool-pre-error')).toContainText('command not found: nope')

  // Still collapsible — the auto-expand is a starting state, not a lock.
  await toolBlock.locator('.tool-header').click()
  await expect(toolBlock.locator('.tool-header')).toHaveAttribute('aria-expanded', 'false')
})

test('a system event with no text renders a label, not a raw JSON blob', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const sessionId = await seedSession(request, auth, 'sysblob', 'mock:system-blob')

  await loadAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty')).toBeVisible({ timeout: 10_000 })

  const sendRes = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth,
    data: { text: 'go', model: 'mock:system-blob' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  const notice = page.getByTestId('chat-system-detail')
  await expect(notice).toBeVisible({ timeout: 10_000 })
  await expect(notice.locator('.chat-unknown-label')).toHaveText('System notice')
  // The payload is behind the <details>, not inline in the feed.
  await expect(notice.locator('.chat-unknown-json')).toBeHidden()
  await notice.locator('summary').click()
  await expect(notice.locator('.chat-unknown-json')).toContainText('mock system blob')
})

test('the first attachment shows an uploading chip, and the icon buttons have names', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const sessionId = await seedSession(request, auth, 'upload')

  await loadAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty')).toBeVisible({ timeout: 10_000 })

  // Item 3: the icon-only attach button is announced by name.
  await expect(page.getByRole('button', { name: 'Attach files' })).toBeVisible()

  // Hold the upload open so the in-flight state is observable.
  let release!: () => void
  const held = new Promise<void>((resolve) => {
    release = resolve
  })
  await page.route('**/api/sessions/*/attachments', async (route) => {
    await held
    await route.continue()
  })

  await page.locator('input[type="file"]').setInputFiles({
    name: 'screenshot.png',
    mimeType: 'image/png',
    buffer: PNG_1X1,
  })

  // Item 2: feedback for the FIRST attachment, while zero chips exist.
  const uploadingChip = page.getByTestId('uploading-chip')
  await expect(uploadingChip).toBeVisible({ timeout: 10_000 })
  await expect(page.locator('.input-bar .attachment-chip-name')).toHaveCount(0)

  release()
  await expect(uploadingChip).toBeHidden({ timeout: 10_000 })
  await expect(page.locator('.input-bar .attachment-chip-name')).toContainText('screenshot.png')

  // Item 3: the per-attachment remove button names its file.
  await expect(page.getByRole('button', { name: 'Remove screenshot.png' })).toBeVisible()
})

test('the folder delete button names the folder it deletes', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const name = `e2e-polish-a11y-${Date.now()}`
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-polish-a11y-'))
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()

  await loadAt(page, token, '/folders')
  // Item 4: announced as "Delete folder <name>", not "times, button".
  await expect(page.getByRole('button', { name: `Delete folder ${name}` })).toBeVisible({
    timeout: 10_000,
  })
})

test('a stale plugin-page URL explains itself and offers a way back', async ({ request, page }) => {
  const { token } = await authenticate(request)

  await loadAt(page, token, '/plugin-page/ghost-plugin/ghost-page')

  const missing = page.getByTestId('plugin-page-missing')
  await expect(missing).toBeVisible({ timeout: 10_000 })
  await expect(missing).toContainText('That page is no longer available')

  await page.getByTestId('plugin-page-missing-back').click()
  await expect(page).toHaveURL(/\/$/)
  await expect(page.locator('.list-view')).toBeVisible()
})

test('a failed report download surfaces the failure instead of doing nothing', async ({
  request,
  page,
}) => {
  const { token } = await authenticate(request)
  const folder = '2026-07-27'
  const file = `polish-download-${Date.now()}.md`
  writeReportFile(folder, file, 'Polish Download')

  await loadAt(page, token, `/reports/${folder}/${file}`)
  await expect(page.locator('.report-viewer-title')).toHaveText('Polish Download', {
    timeout: 10_000,
  })

  // Hold the download so the pending label is observable, then fail it.
  let release!: () => void
  const held = new Promise<void>((resolve) => {
    release = resolve
  })
  await page.route('**/download', async (route) => {
    await held
    await route.fulfill({ status: 500, body: 'nope' })
  })

  const download = page.getByTestId('report-download')
  await download.click()
  await expect(download).toHaveText('Downloading…')
  await expect(download).toBeDisabled()

  release()
  const error = page.getByTestId('report-download-error')
  await expect(error).toBeVisible({ timeout: 10_000 })
  await expect(error).toContainText("Couldn't download this report")
  // The control comes back so the user can retry.
  await expect(download).toHaveText('Download')
  await expect(download).toBeEnabled()
})

test('the pre-hatcher model is a searchable picker and the choice persists', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  // The setting is server-side and survives retries — start from the default.
  const reset = await request.put('/api/settings/pre-hatcher', {
    headers: { ...auth, 'Content-Type': 'application/json' },
    data: { model: '' },
  })
  expect(reset.ok(), `reset pre-hatcher failed: ${await reset.text()}`).toBeTruthy()
  await loadAt(page, token, '/')

  await expect(page.locator('.rail-brand')).toBeVisible({ timeout: 10_000 })
  await page.locator('.rail-avatar').click()
  await page.locator('.user-menu-dropdown').getByRole('menuitem', { name: 'Settings' }).click()
  const settingsPage = page.getByTestId('settings-page')
  await expect(settingsPage).toBeVisible()
  await settingsPage.getByTestId('settings-nav-chat').click()

  const picker = settingsPage.getByTestId('prehatch-model')
  await expect(picker).toBeVisible()
  await expect(picker).toContainText('Auto')
  // Searchable: type a fragment, the catalogue narrows.
  await picker.click()
  const search = page.getByTestId('prehatch-model-search')
  await expect(search).toBeVisible()
  await expect(page.getByRole('option', { name: 'Mock: echo' })).toBeVisible()
  await search.fill('happy')
  await expect(page.getByRole('option', { name: 'Mock: happy path' })).toBeVisible()
  await expect(page.getByRole('option', { name: 'Mock: echo' })).toHaveCount(0)
  await page.getByRole('option', { name: 'Mock: happy path' }).click()
  await expect(picker).toContainText('Mock: happy path')

  // Persisted server-side, so a reload comes back with the same choice.
  await page.reload()
  await expect(page.locator('.rail-brand')).toBeVisible({ timeout: 10_000 })
  await page.locator('.rail-avatar').click()
  await page.locator('.user-menu-dropdown').getByRole('menuitem', { name: 'Settings' }).click()
  await settingsPage.getByTestId('settings-nav-chat').click()
  await expect(settingsPage.getByTestId('prehatch-model')).toContainText('Mock: happy path')

  // Restore the default so other specs see a clean setting.
  await settingsPage.getByTestId('prehatch-model').click()
  await page.getByTestId('prehatch-model-option-default').click()
  await expect(settingsPage.getByTestId('prehatch-model')).toContainText('Auto')
})
