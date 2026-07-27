import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e test: exec-like tool calls render the REAL command line as the
 * row's primary text (never the tool name, never `mcp__…`), followed by
 * the model's one-sentence reason.
 *
 * Driven by `mock:run-command` (an `mcp__peckboard__run_command` call with
 * command, args and `reason`) and `mock:happy-path` (a native `Bash` call
 * whose `description` doubles as the reason).
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
  name: string,
): Promise<{ sessionId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-cmd-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name, folder_id: folder.id },
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

test('run_command rows show the command line and the reason', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedSession(request, authHeader, 'e2e-cmd-display')

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-bubble').first())).toBeVisible({
    timeout: 10_000,
  })

  const sendRes = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'go', model: 'mock:run-command' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  const toolBlock = page.locator('.tool-block').first()
  await expect(toolBlock).toBeVisible({ timeout: 10_000 })

  // Reason-first: the model's why is the primary text; the raw command
  // follows, dimmed — and never the tool name.
  await expect(toolBlock.locator('.tool-reason-primary')).toHaveText(
    'Build the release binary to verify the change compiles.',
  )
  await expect(toolBlock.locator('.tool-cmd')).toHaveText('cargo build --release')
  await expect(toolBlock).not.toContainText('run_command')
  await expect(toolBlock).not.toContainText('run command')
  await expect(toolBlock).not.toContainText('mcp__')
  // Expanding reveals the full command with a Copy button (via ClampedPre).
  await toolBlock.locator('.tool-header').click()
  const cmdSection = toolBlock.locator('.tool-section', {
    has: page.locator('.tool-section-label', { hasText: /^Command$/ }),
  })
  await expect(cmdSection).toContainText('cargo build --release')
  await expect(cmdSection.getByRole('button', { name: 'Copy' })).toBeVisible()
  await page.screenshot({ path: 'test-results/tool-command-display.png', fullPage: true })
})

test('native shell (Bash) rows show the command line and the description', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedSession(request, authHeader, 'e2e-bash-display')

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-bubble').first())).toBeVisible({
    timeout: 10_000,
  })

  const sendRes = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'go', model: 'mock:happy-path' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  const toolBlock = page.locator('.tool-block').first()
  await expect(toolBlock).toBeVisible({ timeout: 10_000 })

  await expect(toolBlock.locator('.tool-reason-primary')).toHaveText(
    'Say hello to prove the shell works.',
  )
  await expect(toolBlock.locator('.tool-cmd')).toHaveText('echo hello')
  await expect(toolBlock).not.toContainText('Terminal')
})
