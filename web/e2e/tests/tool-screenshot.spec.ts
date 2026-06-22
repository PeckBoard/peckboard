import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e test: when a tool returns an image (e.g. the Playwright MCP's
 * `browser_take_screenshot`), the chat must render the screenshot inline as
 * a thumbnail, and clicking the thumbnail must open the full image in a
 * lightbox.
 *
 * Driven by the `mock:screenshot` scenario, which emits a `ToolEnd` carrying
 * a tiny inline PNG. Before this feature the parser dropped array-form
 * `tool_result` content, so the screenshot never reached the UI at all.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type AuthBundle = {
  token: string
  authHeader: { Authorization: string }
}

async function authenticate(request: APIRequestContext): Promise<AuthBundle> {
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
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-shot-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-screenshot', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'screenshot render', folder_id: folder.id },
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

test('tool screenshots render inline and open in a lightbox', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedSession(request, authHeader)

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  await expect(page.locator('.chat-empty').or(page.locator('.chat-bubble').first())).toBeVisible({
    timeout: 10_000,
  })

  const sendRes = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'go', model: 'mock:screenshot' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  // The screenshot thumbnail should appear under the tool block.
  const thumb = page.getByTestId('tool-image-thumb').first()
  await expect(thumb).toBeVisible({ timeout: 10_000 })

  // The thumbnail must carry a decodable data: URL for the PNG.
  const thumbImg = thumb.locator('img')
  await expect(thumbImg).toHaveAttribute('src', /^data:image\/png;base64,/)

  // No lightbox until the user clicks.
  await expect(page.getByTestId('tool-image-lightbox')).toHaveCount(0)

  // Clicking the preview opens the full image in a lightbox.
  await thumb.click()
  const lightbox = page.getByTestId('tool-image-lightbox')
  await expect(lightbox).toBeVisible({ timeout: 5_000 })
  await expect(lightbox.locator('img.image-lightbox-img')).toHaveAttribute(
    'src',
    /^data:image\/png;base64,/,
  )

  // Pressing Escape dismisses the lightbox (shared Modal behaviour).
  await page.keyboard.press('Escape')
  await expect(page.getByTestId('tool-image-lightbox')).toHaveCount(0)
})
