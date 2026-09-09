import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * Codex CLI missing-binary UX: Settings → Providers install-in-session, and
 * the setup-wizard yes/no ask. Mirrors mcp-command-install.spec.ts.
 *
 * The dispatch POST is stubbed so no agent is spawned. The check-command
 * probe is real unless a spec stubs it (present-binary case).
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const WIZARD_PASS = 'codex-wizard-pass-5678'
const INSTALL_SH = 'https://chatgpt.com/codex/install.sh'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

async function loadAs(page: Page, token: string) {
  await page.addInitScript((injectedToken) => {
    if (!localStorage.getItem('peckboard_token')) {
      localStorage.setItem('peckboard_token', injectedToken)
    }
  }, token)
  await page.goto('/')
}

async function stubSetupIncomplete(page: Page) {
  await page.route('**/api/settings/setup', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ completed: false }),
    }),
  )
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

test('wizard: Codex enabled + CLI missing → Yes starts install session', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const probe = await probeCodex(request, token)
  test.skip(probe.found, 'codex is on PATH; missing-install UI cannot be asserted')

  let passwordChanged = false
  try {
    await stubSetupIncomplete(page)
    await loadAs(page, token)

    const wizard = page.getByTestId('setup-wizard')
    await expect(wizard).toBeVisible({ timeout: 10_000 })

    await page.getByTestId('setup-pw-current').fill(E2E_PASS)
    await page.getByTestId('setup-pw-new').fill(WIZARD_PASS)
    await page.getByTestId('setup-pw-confirm').fill(WIZARD_PASS)
    await page.getByTestId('setup-next').click()
    passwordChanged = true

    const toggle = page.getByTestId('setup-provider-toggle-codex')
    await expect(toggle).toBeVisible({ timeout: 10_000 })
    await expect(toggle).toBeChecked()

    let sentText: string | null = null
    await page.route('**/api/sessions/*/message', async (route) => {
      sentText = (route.request().postDataJSON() as { text?: string })?.text ?? ''
      await route.fulfill({ contentType: 'application/json', body: '{}' })
    })

    await page.getByTestId('setup-next').click()
    const dialog = page.getByTestId('codex-install-dialog')
    await expect(dialog).toBeVisible({ timeout: 10_000 })
    await expect(dialog).toContainText('Codex CLI is not installed. Install it now?')

    await page.getByTestId('confirm-dialog-confirm').click()

    await expect(page.getByTestId('setup-default-model')).toBeVisible({ timeout: 15_000 })
    expect(sentText).toBeTruthy()
    expect(sentText!).toContain(INSTALL_SH)
    expect(sentText!).toContain('sudo -A')
    expect(sentText!).toContain('the Codex provider')
  } finally {
    if (passwordChanged) {
      const res = await request.post('/api/auth/login', {
        data: { username: E2E_USER, password: WIZARD_PASS },
      })
      if (res.ok()) {
        const { token: fresh } = (await res.json()) as { token: string }
        const change = await request.post('/api/auth/change-password', {
          headers: { Authorization: `Bearer ${fresh}` },
          data: { current_password: WIZARD_PASS, new_password: E2E_PASS },
        })
        expect(change.ok(), `restore password failed: ${await change.text()}`).toBeTruthy()
      }
    }
  }
})
