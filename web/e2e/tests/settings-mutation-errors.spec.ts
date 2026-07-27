import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * A settings mutation the server refuses must never look like it stuck.
 *
 * Every case here forces the write to fail (mocked 4xx/5xx) and asserts
 * the two halves of the contract: the control goes back to the value the
 * server actually holds, and the reason is readable inline in the section
 * that owns the control. One representative case per file:
 *
 * 1. `SettingsPage` — caveman mode (optimistic local state + PUT).
 * 2. `PluginsSection` — plugin approve and uninstall from the details modal.
 * 3. `ClaudeAccountsSection` — account delete through the store.
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

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

test('a refused caveman-mode save reverts the control and shows the reason', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)

  // GET reports the stored level; PUT is refused.
  await page.route('**/api/settings/caveman', async (route) => {
    if (route.request().method() === 'PUT') {
      await route.fulfill({
        status: 500,
        contentType: 'application/json',
        body: JSON.stringify({ error: 'settings store is read-only' }),
      })
      return
    }
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({ level: 'off' }),
    })
  })

  await loadAppAt(page, token, '/settings')
  await page.getByTestId('settings-nav-chat').click()
  const section = page.getByTestId('caveman-section')
  await expect(section).toBeVisible({ timeout: 10_000 })
  await expect(section.locator('button.theme-btn.active')).toHaveText('Off')

  await section.getByRole('button', { name: 'Full' }).click()

  const error = page.getByTestId('settings-error-caveman')
  await expect(error).toBeVisible({ timeout: 5_000 })
  await expect(error).toContainText('settings store is read-only')
  // Reverted: the server still holds "off", so that's what the page shows.
  await expect(section.locator('button.theme-btn.active')).toHaveText('Off')
  await page.screenshot({ path: 'e2e/test-results/settings-caveman-save-failed.png' })
})

test('a refused plugin approval keeps the modal open; a refused remove keeps the row', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)

  // A denied plugin: the startup approval prompt only claims `pending`
  // ones, so the details modal is the surface under test here.
  await page.route('**/api/plugins', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        plugins: [],
        ui_panels: [],
        wasm_plugins: [
          {
            name: 'nginx-manager',
            description: 'Manage an Nginx Proxy Manager instance.',
            version: '0.2.0',
            repository: 'https://github.com/PeckBoard/nginx-manager',
            hooks: ['mcp.tool.invoke'],
            permissions: ['provide_mcp_tools'],
            status: 'denied',
            error: null,
          },
        ],
      }),
    })
  })
  await page.route('**/api/plugins/nginx-manager/approval', async (route) => {
    await route.fulfill({
      status: 500,
      contentType: 'application/json',
      body: JSON.stringify({ error: 'approval store is read-only' }),
    })
  })
  await page.route('**/api/plugins/nginx-manager', async (route) => {
    await route.fulfill({
      status: 409,
      contentType: 'application/json',
      body: JSON.stringify({ error: 'plugin file is locked' }),
    })
  })

  await loadAppAt(page, token, '/plugins')
  const row = page.getByTestId('wasm-plugin-nginx-manager')
  await expect(row).toBeVisible({ timeout: 10_000 })

  await row.getByTestId('wasm-plugin-open-nginx-manager').click()
  const details = page.getByTestId('plugin-details-nginx-manager')
  await expect(details).toBeVisible()
  await details.getByTestId('wasm-plugin-approve-nginx-manager').click()

  // The modal stays open with the reason: nothing was approved.
  await expect(details).toBeVisible()
  await expect(details.getByTestId('plugin-details-error')).toContainText(
    'approval store is read-only',
  )
  await expect(row).toHaveAttribute('data-status', 'denied')
  await page.screenshot({ path: 'e2e/test-results/plugin-approval-failed.png' })

  await details.locator('.form-actions .btn-secondary').click()
  await expect(details).toHaveCount(0)

  // Same contract for uninstall: the row survives and the list says why.
  await row.getByTestId('wasm-plugin-remove-nginx-manager').click()
  await page.locator('.confirm-dialog').getByRole('button', { name: 'Remove' }).click()
  await expect(page.getByTestId('wasm-plugin-error')).toContainText('plugin file is locked')
  await expect(row).toBeVisible()
})

test('a refused Claude account delete keeps the row and shows the reason', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)

  const account = {
    id: 'acct-e2e',
    name: 'Work login',
    kind: 'oauth_token',
    credential_hint: 'sk-…7f2a',
    config_dir: null,
    budget_window_hours: null,
    budget_limit_usd: null,
    budget_limit_tokens: null,
    warn_threshold: 0.8,
    critical_threshold: 0.95,
    created_at: 0,
    updated_at: 0,
    usage: { total_tokens: 0, est_cost_usd: 0, turns: 0, used_fraction: null, level: 'none' },
  }

  await page.route('**/api/claude-accounts', async (route) => {
    await route.fulfill({ contentType: 'application/json', body: JSON.stringify([account]) })
  })
  await page.route('**/api/claude-accounts/acct-e2e', async (route) => {
    await route.fulfill({
      status: 409,
      contentType: 'application/json',
      body: JSON.stringify({ error: 'account is pinned by a running session' }),
    })
  })

  await loadAppAt(page, token, '/settings')
  await page.getByTestId('settings-nav-providers').click()
  const section = page.getByTestId('claude-accounts-section')
  await expect(section).toBeVisible({ timeout: 10_000 })
  const row = section.getByTestId('acct-row-acct-e2e')
  await expect(row).toBeVisible()

  await row.getByTestId('acct-delete-acct-e2e').click()
  await page.locator('.confirm-dialog').getByRole('button', { name: 'Delete' }).click()

  // The delete was refused: the row is still there and the section says why.
  await expect(section.locator('.form-error')).toContainText(
    'account is pinned by a running session',
  )
  await expect(row).toBeVisible()
  await page.screenshot({ path: 'e2e/test-results/claude-account-delete-failed.png' })
})
