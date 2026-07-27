import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * A failed send must not eat the user's work.
 *
 * The composer clears itself optimistically, so a POST that fails has to
 * put everything back — the text AND the already-uploaded attachments —
 * and say why. Before this, `setAttachments([])` had already run and the
 * files were gone for good with no explanation on screen.
 *
 * We fail the first `POST /message` with `page.route`, then let the
 * second one through, so the retry path is exercised end to end against
 * the real server with a `mock:*` model.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

// A 1x1 transparent PNG — small but a genuine image, so the upload is
// accepted and the attachment survives the round trip.
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
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

async function seedSession(
  request: APIRequestContext,
  authHeader: Record<string, string>,
): Promise<{ sessionId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-sendfail-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-sendfail', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'send failure', folder_id: folder.id, model: 'mock:happy-path' },
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
}

test('a failed send keeps the text and attachments, explains itself, and retries', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedSession(request, authHeader)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty')).toBeVisible({ timeout: 10_000 })

  // Attach an image and type a caption.
  await page.locator('input[type="file"]').setInputFiles({
    name: 'diagram.png',
    mimeType: 'image/png',
    buffer: PNG_1X1,
  })
  const chip = page.locator('.input-bar .attachment-chip-name')
  await expect(chip).toContainText('diagram.png', { timeout: 10_000 })
  await page.locator('.input-textarea').fill('please look at this')

  // Fail exactly the first send.
  let failed = 0
  await page.route('**/api/sessions/*/message', async (route) => {
    failed += 1
    await route.fulfill({
      status: 500,
      contentType: 'application/json',
      body: JSON.stringify({ error: 'The session backend is unavailable.' }),
    })
  })

  await page.locator('button[aria-label="Send message"]').click()

  // The alert explains the failure and offers a retry…
  const alert = page.getByTestId('send-error')
  await expect(alert).toBeVisible({ timeout: 10_000 })
  await expect(alert).toContainText('The session backend is unavailable.')
  expect(failed).toBe(1)

  // …and nothing was lost: the draft and the attachment chip are back.
  await expect(page.locator('.input-textarea')).toHaveValue('please look at this')
  await expect(chip).toContainText('diagram.png')
  // The optimistic bubble is gone — the message was never accepted.
  await expect(page.locator('.chat-bubble-user')).toHaveCount(0)

  // Let the retry through.
  await page.unroute('**/api/sessions/*/message')
  await page.getByTestId('send-error-retry').click()

  await expect(page.locator('.chat-bubble-user')).toContainText('please look at this', {
    timeout: 10_000,
  })
  const indicator = page.getByTestId('message-attachments')
  await expect(indicator).toBeVisible({ timeout: 10_000 })
  // An image attachment renders as a thumbnail, so the filename lives on
  // the button's label rather than in the chip text.
  await expect(indicator.getByLabel('Open diagram.png')).toBeVisible()
  await expect(alert).toHaveCount(0)
  // The composer emptied for real this time.
  await expect(page.locator('.input-textarea')).toHaveValue('')
  await expect(page.locator('.input-bar .attachment-chip-name')).toHaveCount(0)
})
