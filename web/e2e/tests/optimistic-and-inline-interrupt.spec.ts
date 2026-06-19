import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e for the chat-view papercuts:
 *
 *   1. Sending a message renders an optimistic user bubble immediately
 *      (so a queued message doesn't appear to vanish into the WS
 *      round-trip).
 *   2. The Interrupt button lives inline next to the "Thinking..."
 *      indicator at the end of the chat, NOT as its own bottom toolbar.
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
  label: string,
): Promise<{ sessionId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-${label}-`))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-${label}`, path: folderPath },
  })
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: label, folder_id: folder.id },
  })
  const session = (await sessionRes.json()) as { id: string }
  return { sessionId: session.id }
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

test('Interrupt button renders inline next to the Thinking indicator', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL).toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedSession(request, authHeader, 'inline-interrupt')

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-bubble').first())).toBeVisible({
    timeout: 10_000,
  })

  // mock:ask hangs waiting for stdin, giving us a stable "agent is
  // working" window to interrogate the UI.
  await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'hi', model: 'mock:ask' },
  })

  // The inline Interrupt chip must appear inside the Thinking row…
  const thinkingRow = page.locator('.chat-thinking')
  await expect(thinkingRow).toBeVisible({ timeout: 10_000 })
  await expect(thinkingRow.locator('.chat-thinking-interrupt')).toBeVisible()

  // …and the old standalone interrupt bar must NOT exist anymore.
  await expect(page.locator('.chat-interrupt-bar')).toHaveCount(0)
  await expect(page.locator('.chat-interrupt-btn')).toHaveCount(0)
})

test('Sending a message shows an optimistic user bubble before the WS round-trip', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL).toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedSession(request, authHeader, 'optimistic')

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-bubble').first())).toBeVisible({
    timeout: 10_000,
  })

  // Slow down /message so the optimistic bubble is observable for a
  // noticeable window before the real WS event arrives. Without this
  // the round-trip can finish before Playwright polls.
  await page.route('**/api/sessions/*/message', async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 400))
    await route.continue()
  })

  const composer = page.locator('.input-textarea')
  await composer.fill('hello from the past')
  await page.locator('.send-btn').click()

  // The optimistic bubble class is on the same chat-bubble-user
  // element; assert both the text and the pending marker are present.
  const bubble = page.locator('.chat-bubble-user', { hasText: 'hello from the past' })
  await expect(bubble).toBeVisible({ timeout: 1_000 })

  // The composer must have cleared and the "Sending..." sub-label
  // tells the user the message is in flight.
  await expect(composer).toHaveValue('')
  await expect(
    page.locator('.chat-bubble-pending', { hasText: 'hello from the past' }),
  ).toHaveCount(1)

  // Once the WS confirms the real event, the optimistic bubble loses
  // its `chat-bubble-pending` class — we should be left with exactly
  // one user bubble of that text.
  await expect(page.locator('.chat-bubble-pending')).toHaveCount(0, { timeout: 10_000 })
  await expect(page.locator('.chat-bubble-user', { hasText: 'hello from the past' })).toHaveCount(1)
})
