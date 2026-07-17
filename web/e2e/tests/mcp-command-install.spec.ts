import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * UI e2e for the stdio MCP "missing host binary" flow and the sudo askpass
 * dialog:
 *
 *  1. Adding a stdio server whose `command` is absent on the host shows a
 *     warning with install steps + an "Install in a session" button (the
 *     check-command endpoint is exercised for real).
 *  2. A present command (`sh`) shows no warning.
 *  3. "Install in a session" creates a temp session and opens its tab
 *     (folder + session creation real; the dispatch POST is stubbed so no
 *     agent is spawned).
 *  4. The global askpass dialog renders on `peckboard:askpass-request`,
 *     submits the password to the answer endpoint, and dismisses on
 *     `peckboard:askpass-resolved`.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

async function openMcpSettings(page: Page, token: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/settings')
  await page.getByTestId('settings-nav-mcp').click()
  await expect(page.getByTestId('mcp-servers-section')).toBeVisible({ timeout: 10_000 })
}

test('stdio missing-binary warning + install-in-session', async ({ request, page }) => {
  const token = await authenticate(request)
  await request.put('/api/settings/mcp-servers', {
    data: { servers: [] },
    headers: { Authorization: `Bearer ${token}` },
  })

  await openMcpSettings(page, token)
  await page.getByTestId('mcp-add-server').click()
  await expect(page.getByTestId('mcp-server-modal')).toBeVisible()

  // A present binary → no warning.
  await page.getByTestId('mcp-field-name').fill('present')
  await page.getByTestId('mcp-field-command').fill('sh')
  await expect(page.getByTestId('mcp-cmd-warning')).toHaveCount(0, { timeout: 5_000 })

  // A missing binary → warning + install steps + button.
  await page.getByTestId('mcp-field-command').fill('definitely-not-a-real-binary-xyz')
  const warning = page.getByTestId('mcp-cmd-warning')
  await expect(warning).toBeVisible({ timeout: 5_000 })
  await expect(warning).toContainText('not found')
  await expect(warning).toContainText('definitely-not-a-real-binary-xyz')
  await expect(page.getByTestId('mcp-install-in-session')).toBeVisible()
  await expect(page.getByTestId('mcp-install-in-session')).toBeVisible()
  await page.screenshot({ path: 'e2e/test-results/mcp-cmd-warning.png' })
  // agent — but assert the install prompt carries the sudo -A rule.
  let sentText: string | null = null
  await page.route('**/api/sessions/*/message', async (route) => {
    sentText = (route.request().postDataJSON() as { text?: string })?.text ?? ''
    await route.fulfill({ contentType: 'application/json', body: '{}' })
  })

  await page.getByTestId('mcp-install-in-session').click()

  // The temp install session opens as the active tab.
  await expect(page.getByText('Install definitely-not-a-real-binary-xyz').first()).toBeVisible({
    timeout: 15_000,
  })
  expect(sentText).toBeTruthy()
  expect(sentText!).toContain('sudo -A')
  expect(sentText!).toContain('--version')
})

test('askpass password dialog round-trip', async ({ request, page }) => {
  const token = await authenticate(request)
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/')
  // Wait for the app to be interactive (WS connected / sidebar present).
  await expect(page.getByTestId('askpass-dialog')).toHaveCount(0)

  // Capture the answer POST instead of hitting the (empty) registry.
  let answerBody: { request_id?: string; password?: string } | null = null
  await page.route('**/api/sessions/*/askpass-answer', async (route) => {
    answerBody = route.request().postDataJSON()
    await route.fulfill({ contentType: 'application/json', body: '{"ok":true}' })
  })

  // Simulate the WS-fanned request event the dialog listens for.
  await page.evaluate(() => {
    window.dispatchEvent(
      new CustomEvent('peckboard:askpass-request', {
        detail: {
          request_id: 'req-e2e-1',
          session_id: 'sess-e2e-1',
          prompt: '[sudo] password for e2e:',
        },
      }),
    )
  })

  const dialog = page.getByTestId('askpass-dialog')
  await expect(dialog).toBeVisible()
  await expect(dialog).toContainText('[sudo] password for e2e:')
  await page.screenshot({ path: 'e2e/test-results/askpass-dialog.png' })

  await page.getByTestId('askpass-input').fill('s3cr3t-pw')
  await page.getByTestId('askpass-submit').click()

  await expect(dialog).toHaveCount(0)
  expect(answerBody).toBeTruthy()
  expect(answerBody!.request_id).toBe('req-e2e-1')
  expect(answerBody!.password).toBe('s3cr3t-pw')
})
