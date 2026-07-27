import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e for failed session-control actions in the chat view.
 *
 * Clear / Terminate / Delete used to close their confirm dialog *before*
 * awaiting the request, and the inline Interrupt was a bare unhandled
 * promise — so a refusal looked exactly like a success: dialog gone,
 * nothing changed, no message. These tests pin the recovery shape:
 *
 *   - the confirm dialog stays open and shows a readable reason
 *   - the confirm button is re-enabled, so the same click retries
 *   - retrying against a healthy server completes the action
 *   - a failed interrupt lands in the chat error banner and the button
 *     locks while the request is in flight (no stacked interrupts)
 *
 * Failures are injected with Playwright route interception; the same
 * technique as error-recovery.spec.ts.
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

async function seedSession(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  name: string,
): Promise<{ sessionId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-ctlerr-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-ctlerr-${name}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: `control errors ${name}`, folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  return { sessionId: session.id }
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-bubble').first())).toBeVisible({
    timeout: 10_000,
  })
}

/** Reject every request matching `pattern` with a human-readable body. */
async function failWith(page: Page, pattern: string, message: string, delayMs = 0) {
  await page.route(pattern, async (route) => {
    if (delayMs > 0) await new Promise((r) => setTimeout(r, delayMs))
    await route.fulfill({
      status: 500,
      contentType: 'application/json',
      body: JSON.stringify({ error: message }),
    })
  })
}

async function openSessionMenu(page: Page, itemTestId: string) {
  await page.locator('.chat-toolbar-menu').click()
  await page.getByTestId(itemTestId).click()
}

test('a refused Clear keeps the dialog open with a readable error and retries', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedSession(request, authHeader, 'clear')

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  const pattern = `**/api/sessions/${sessionId}/clear`
  await failWith(page, pattern, 'Clear refused: the transcript is locked.')

  await openSessionMenu(page, 'chat-menu-clear')
  const dialog = page.getByTestId('confirm-clear')
  await expect(dialog).toBeVisible()

  const confirm = page.getByTestId('confirm-dialog-confirm')
  await confirm.click()

  // The failure is spelled out inside the dialog, which is still open.
  await expect(page.getByTestId('confirm-dialog-error')).toHaveText(
    'Clear refused: the transcript is locked.',
  )
  await expect(dialog).toBeVisible()
  // …and the control is live again, so the very same button is the retry.
  await expect(confirm).toBeEnabled()

  await page.unroute(pattern)
  await confirm.click()
  await expect(dialog).toHaveCount(0, { timeout: 10_000 })
})

test('a refused Terminate keeps the dialog open with a readable error', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedSession(request, authHeader, 'terminate')

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  const pattern = `**/api/sessions/${sessionId}/terminate`
  await failWith(page, pattern, 'Terminate refused: the agent is mid-handover.')

  await openSessionMenu(page, 'chat-toolbar-terminate')
  const dialog = page.getByTestId('confirm-terminate')
  await expect(dialog).toBeVisible()

  const confirm = page.getByTestId('confirm-dialog-confirm')
  await confirm.click()

  await expect(page.getByTestId('confirm-dialog-error')).toHaveText(
    'Terminate refused: the agent is mid-handover.',
  )
  await expect(dialog).toBeVisible()
  await expect(confirm).toBeEnabled()

  await page.unroute(pattern)
  await confirm.click()
  await expect(dialog).toHaveCount(0, { timeout: 10_000 })
})

test('a refused Delete keeps the dialog open with a readable error and retries', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedSession(request, authHeader, 'delete')

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  // Only the DELETE verb fails — the session-detail GET on the same URL
  // must keep working or the whole view falls over.
  const pattern = `**/api/sessions/${sessionId}`
  await page.route(pattern, async (route) => {
    if (route.request().method() !== 'DELETE') return route.continue()
    await route.fulfill({
      status: 500,
      contentType: 'application/json',
      body: JSON.stringify({ error: 'Delete refused: this session is owned by a card.' }),
    })
  })

  await openSessionMenu(page, 'chat-menu-delete')
  const dialog = page.getByTestId('confirm-delete')
  await expect(dialog).toBeVisible()

  const confirm = page.getByTestId('confirm-dialog-confirm')
  await confirm.click()

  await expect(page.getByTestId('confirm-dialog-error')).toHaveText(
    'Delete refused: this session is owned by a card.',
  )
  await expect(dialog).toBeVisible()
  await expect(confirm).toBeEnabled()

  await page.unroute(pattern)
  await confirm.click()
  await expect(dialog).toHaveCount(0, { timeout: 10_000 })
})

test('a refused Interrupt surfaces a banner and locks the button while in flight', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedSession(request, authHeader, 'interrupt')

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  // mock:ask blocks on stdin, so the thinking row (and its inline
  // Interrupt chip) stays on screen for the whole test.
  const sendRes = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'ask me', model: 'mock:ask' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  await failWith(
    page,
    `**/api/sessions/${sessionId}/interrupt`,
    'Interrupt refused: no process is attached.',
    600,
  )

  const interrupt = page.locator('.chat-thinking-interrupt')
  await expect(interrupt).toBeVisible({ timeout: 10_000 })
  await interrupt.click()

  // In flight: locked, so a second click can't stack another interrupt.
  await expect(interrupt).toBeDisabled()

  // Failure lands in the chat error banner, in plain words…
  const banner = page.getByTestId('chat-patch-error')
  await expect(banner).toContainText('Interrupt refused: no process is attached.', {
    timeout: 10_000,
  })
  // …and the button is usable again for a retry.
  await expect(interrupt).toBeEnabled()

  await banner.getByRole('button', { name: 'Dismiss' }).click()
  await expect(page.getByTestId('chat-patch-error')).toHaveCount(0)
})
