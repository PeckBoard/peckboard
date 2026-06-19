import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI papercuts covered here:
 *
 *  1. The tab strip exposes a trailing `+` button (far right). Clicking
 *     it opens the New Session modal — the same entry point the rail's
 *     "+ New session" button uses, but reachable from anywhere in the
 *     app without first navigating to the sessions list.
 *
 *  2. `Agent crashed` renders as a plain inline notice (the same
 *     `.chat-agent-start` row used for "Agent started" and "Agent
 *     interrupted"), NOT as a boxed `.chat-system-notice` with the ℹ️
 *     icon. Driven by `mock:crash` so the assertion is deterministic.
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
  prefix: string,
): Promise<{ sessionId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-${prefix}-`))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-${prefix}-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: `${prefix} session`, folder_id: folder.id },
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

test('tab bar exposes a trailing "+" button that opens the New Session modal', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  // Seed at least one session so the tab strip definitely renders with
  // a real chip alongside the new `+` button.
  const { sessionId } = await seedSession(request, authHeader, 'tab-new')

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })

  const newBtn = page.locator('.tab-new')
  await expect(newBtn).toBeVisible()
  await expect(newBtn).toHaveText('+')

  // The button must sit AFTER all opened-tab chips in DOM order (far
  // right of the strip).
  const lastChild = page.locator('.tabbar > *').last()
  await expect(lastChild).toHaveClass(/tab-new/)

  await newBtn.click()
  await expect(page.locator('.modal, .new-session-modal').first()).toBeVisible({
    timeout: 5_000,
  })
})

test('Agent crashed renders as a plain notice, not a boxed system notice', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedSession(request, authHeader, 'crash-ui')

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-bubble').first())).toBeVisible({
    timeout: 10_000,
  })

  // mock:crash → Started → Text → Crashed
  const sendRes = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'go', model: 'mock:crash' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  // The plain "Agent crashed" notice should appear in the same
  // `.chat-agent-start` row used by Agent started / interrupted.
  const crashLabel = page.locator('.chat-agent-start-label').filter({ hasText: 'Agent crashed' })
  await expect(crashLabel).toBeVisible({ timeout: 15_000 })

  // The reason chip should sit next to the label in the same row.
  // Scope by the parent row so the assertion isn't ambiguous with the
  // `Agent started` row's model-name chip earlier in the chat.
  const crashRow = page.locator('.chat-agent-start').filter({ has: crashLabel })
  await expect(crashRow.locator('.chat-agent-start-detail')).toContainText(/crash/i)

  // Crucially: the boxed `.chat-system-notice` (with the ℹ️ icon) must
  // NOT wrap the crash notice. That was the bubble the user wanted gone.
  await expect(page.locator('.chat-system-notice', { hasText: /Agent crashed/i })).toHaveCount(0)
  await expect(page.locator('.chat-system-notice-icon')).toHaveCount(0)
})
