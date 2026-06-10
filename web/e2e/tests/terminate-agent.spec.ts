import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e test for the "Terminate agent" kebab-menu action.
 *
 * The button exists so users can kill the long-lived provider child
 * between turns — the next message then spawns a fresh process, which
 * picks up any new skills, MCP config, etc. Distinct from /interrupt
 * (which is the inline stop-the-turn button shown only while the agent
 * is thinking).
 *
 * Asserts the full flow end-to-end: open kebab → click menu item →
 * confirm dialog → "Agent terminated" system notice lands in transcript.
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
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-term-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-terminate', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'terminate UI', folder_id: folder.id },
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

test('Terminate agent menu item shows a confirm dialog and posts a system notice', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedSession(request, authHeader)

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  // Wait for the session shell to render.
  await expect(page.locator('.chat-empty').or(page.locator('.chat-bubble').first())).toBeVisible({
    timeout: 10_000,
  })

  // Open the kebab menu.
  await page.locator('.chat-toolbar-menu').click()

  // Click the Terminate agent entry.
  await page.getByTestId('chat-toolbar-terminate').click()

  // Confirm dialog must surface with the warning copy.
  await expect(page.locator('.confirm-dialog-title')).toHaveText('Terminate agent')
  await expect(page.locator('.confirm-dialog-message')).toContainText(/fresh process/i)

  // Confirm.
  await page.locator('.confirm-dialog .btn-primary').click()

  // The system notice must appear in the transcript. The route appends a
  // `system` event whose text begins "Agent terminated"; the UI renders
  // it inside .chat-system-notice (no report metadata → plain branch).
  await expect(page.locator('.chat-system-notice')).toContainText(/Agent terminated/i, {
    timeout: 10_000,
  })

  // Sanity: a "crashed" banner must not appear — terminate on an idle
  // session has no in-flight turn for the provider to wind down, so
  // there's no synthetic Crashed event to render.
  await expect(page.getByText(/Agent crashed/i)).toHaveCount(0)
})
