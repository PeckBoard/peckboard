import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e test: when the user clicks the Interrupt button, the chat must
 * show a subtle "Agent interrupted" line — not the boxed "Agent crashed"
 * banner. The provider stream emits a `Crashed { reason: "interrupted" }`
 * event as it winds down; pairing that with the route-emitted `interrupt`
 * event used to surface as an error in the UI.
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
): Promise<{ sessionId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-irq-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-interrupt', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'interrupt UI', folder_id: folder.id },
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

test('clicking Interrupt shows a subtle notice, not a crash banner', async ({
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

  // mock:ask blocks waiting for stdin — keeps the agent running long
  // enough for the user to hit Interrupt.
  const sendRes = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'ask me', model: 'mock:ask' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  // Wait until the agent-start notice is rendered (proxy for "agent is running").
  await expect(
    page.locator('.chat-agent-start-label').filter({ hasText: 'Agent started' }),
  ).toBeVisible({ timeout: 10_000 })

  // Click the Interrupt button.
  await page.locator('.chat-interrupt-btn').click()

  // The "Agent interrupted" subtle notice must appear.
  await expect(
    page.locator('.chat-agent-start-label').filter({ hasText: 'Agent interrupted' }),
  ).toBeVisible({ timeout: 10_000 })

  // The crash banner (boxed system notice) must NOT appear.
  await expect(page.getByText(/Agent crashed/i)).toHaveCount(0)

  // And the interrupted notice should NOT be wrapped in the boxed
  // chat-system-notice container — it should sit in the subtle
  // chat-agent-start layout instead.
  await expect(page.locator('.chat-system-notice', { hasText: /interrupt/i })).toHaveCount(0)
})
