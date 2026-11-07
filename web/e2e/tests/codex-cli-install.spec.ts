import { test, expect, type APIRequestContext, type Page } from '../harness'

/**
 * Codex CLI missing-binary UX: Settings → Providers install-in-session.
 * Mirrors mcp-command-install.spec.ts. The setup wizard installs CLIs without
 * an agent; see provider-setup.spec.ts.
 *
 * The dispatch POST is stubbed so no agent is spawned. The check-command
 * probe is real unless a spec stubs it (present-binary case).
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const INSTALL_SH = 'https://chatgpt.com/codex/install.sh'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

async function probeCodex(request: APIRequestContext, token: string) {
  const res = await request.post('/api/settings/mcp-servers/check-command', {
    headers: { Authorization: `Bearer ${token}` },
    data: { command: 'codex' },
  })
  expect(res.ok(), `check-command failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as { found: boolean; hints: string[] }
}

async function openCodexSettings(page: Page, token: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/settings')
  await page.getByTestId('settings-nav-providers').click()
  await expect(page.getByTestId('codex-settings-section')).toBeVisible({ timeout: 10_000 })
}

test('settings: missing Codex CLI shows install-in-session with official install.sh', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const probe = await probeCodex(request, token)
  test.skip(probe.found, 'codex is on PATH; missing-install UI cannot be asserted')
  expect(probe.hints.join('\n')).toContain(INSTALL_SH)

  await request.put('/api/settings/providers/codex', {
    headers: { Authorization: `Bearer ${token}` },
    data: { hidden: false },
  })

  await openCodexSettings(page, token)

  const warning = page.getByTestId('codex-cmd-warning')
  await expect(warning).toBeVisible({ timeout: 10_000 })
  await expect(warning).toContainText('not found')
  await expect(warning).toContainText(INSTALL_SH)
  await expect(page.getByTestId('codex-install-in-session')).toBeVisible()

  let sentText: string | null = null
  await page.route('**/api/sessions/*/message', async (route) => {
    sentText = (route.request().postDataJSON() as { text?: string })?.text ?? ''
    await route.fulfill({ contentType: 'application/json', body: '{}' })
  })

  await page.getByTestId('codex-install-in-session').click()

  // `getByText('Install codex')` also matches the warning's "Install Codex CLI"
  // hint — wait on the intercepted prompt instead, then the new tab.
  await expect.poll(() => sentText, { timeout: 15_000 }).toBeTruthy()
  await expect(page.getByRole('tab', { name: 'Install codex' })).toBeVisible({ timeout: 10_000 })
  expect(sentText!).toContain(INSTALL_SH)
  expect(sentText!).toContain('sudo -A')
  expect(sentText!).toContain('the Codex provider')
  expect(sentText!).not.toContain('MCP server')
})

test('settings: present Codex CLI hides the missing-binary warning', async ({ request, page }) => {
  const token = await authenticate(request)
  await request.put('/api/settings/providers/codex', {
    headers: { Authorization: `Bearer ${token}` },
    data: { hidden: false },
  })
  await page.route('**/api/settings/mcp-servers/check-command', async (route) => {
    const body = route.request().postDataJSON() as { command?: string }
    const name = body.command ?? ''
    if (name === 'codex' || name.endsWith('/codex')) {
      await route.fulfill({
        contentType: 'application/json',
        body: JSON.stringify({
          found: true,
          resolved_path: '/usr/bin/codex',
          hints: [],
          suggested_folder_path: '/tmp/peckboard-installs/codex',
        }),
      })
      return
    }
    await route.continue()
  })
  await openCodexSettings(page, token)
  await expect(page.getByTestId('plugin-settings-codex')).toBeVisible({ timeout: 10_000 })
  await expect(page.getByTestId('codex-cmd-warning')).toHaveCount(0, { timeout: 5_000 })
})
