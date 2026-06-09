import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e test: clearing a session while the agent is still running must
 * leave the chat genuinely empty. The provider stream emits a synthetic
 * `Crashed { reason: "interrupted" }` as it winds down on cancel; if the
 * route deletes events before waiting for that emission, the Crashed
 * lands AFTER the wipe and resurrects an "Agent crashed (interrupted)"
 * line on the cleared session.
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
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-clear-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-clear', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'clear UI', folder_id: folder.id },
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

test('clearing a running session leaves no crash banner behind', async ({
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

  // mock:ask blocks waiting for stdin so the agent is still running
  // when we hit /clear — that's the path that emits the synthetic
  // Crashed event from the cancel branch.
  const sendRes = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'ask me', model: 'mock:ask' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  await expect(
    page.locator('.chat-agent-start-label').filter({ hasText: 'Agent started' }),
  ).toBeVisible({ timeout: 10_000 })

  // Clear via the API (same path the toolbar uses, minus the confirm modal).
  const clearRes = await request.post(`/api/sessions/${sessionId}/clear`, {
    headers: authHeader,
  })
  expect(clearRes.ok(), `clear failed: ${await clearRes.text()}`).toBeTruthy()

  // After the wipe the chat should be empty — no crash banner from a
  // late-arriving Crashed event, no interrupted notice from the pre-cancel
  // interrupt row, nothing at all.
  await expect(page.locator('.chat-empty')).toBeVisible({ timeout: 10_000 })
  await expect(page.getByText(/Agent crashed/i)).toHaveCount(0)
  await expect(
    page.locator('.chat-agent-start-label').filter({ hasText: 'Agent started' }),
  ).toHaveCount(0)
  await expect(
    page.locator('.chat-agent-start-label').filter({ hasText: 'Agent interrupted' }),
  ).toHaveCount(0)

  // And the underlying events list must be empty too, so a reload
  // doesn't show the crash either.
  const eventsRes = await request.get(`/api/sessions/${sessionId}/events`, {
    headers: authHeader,
  })
  expect(eventsRes.ok()).toBeTruthy()
  const events = (await eventsRes.json()) as unknown[]
  expect(events).toEqual([])
})
