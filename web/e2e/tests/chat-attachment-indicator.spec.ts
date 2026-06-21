import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e for the "image attached" indicator on chat messages.
 *
 * Regardless of which model consumes the bytes, a sent message that
 * carried an attachment must show an indicator chip on its bubble — the
 * chip is derived from the persisted `user` event's attachment metadata,
 * so it works for every provider. We drive it with `mock:happy-path` so
 * the test never depends on a real agent binary.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

// A 1x1 transparent PNG — small but a genuine image so the backend's mime
// sniffing records `image/png` on the user event.
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
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-attach-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-attach', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    // Pin a mock model so the send doesn't try to spawn a real agent.
    data: { name: 'attach indicator', folder_id: folder.id, model: 'mock:happy-path' },
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

test('a sent message with an image shows the attachment indicator on its bubble', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedSession(request, authHeader)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty')).toBeVisible({ timeout: 10_000 })

  // Attach an image — the composer shows an upload chip once it lands.
  await page.locator('input[type="file"]').setInputFiles({
    name: 'screenshot.png',
    mimeType: 'image/png',
    buffer: PNG_1X1,
  })
  await expect(page.locator('.input-bar .attachment-chip-name')).toContainText('screenshot.png', {
    timeout: 10_000,
  })

  // Type a caption and send.
  await page.locator('.input-textarea').fill('look at this')
  await page.locator('button[aria-label="Send message"]').click()

  // The user bubble carries the caption AND the attachment indicator,
  // and the indicator survives once the persisted `user` event replaces
  // the optimistic bubble.
  const indicator = page.getByTestId('message-attachments')
  await expect(indicator).toBeVisible({ timeout: 10_000 })
  await expect(indicator).toContainText('screenshot.png')
  await expect(page.locator('.chat-bubble-user')).toContainText('look at this')
})
