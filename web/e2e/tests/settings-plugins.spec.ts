import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * UI e2e for the Plugins settings sub-page (Settings → Plugins).
 *
 * The Plugins sub-page renders directly from `GET /api/plugins`, so this
 * is the user-visible counterpart to `tests/plugins_endpoint.rs`. We
 * verify:
 *
 * 1. The `/plugins` deep-link opens Settings → Plugins.
 * 2. The two built-in plugins (`claude-code`, `mock`) show up as compact
 *    rows with their display names and Active badge.
 * 3. Clicking a row opens the details modal carrying the "Built-in ·
 *    always enabled" tag and the permissions the plugin was granted (the
 *    wire shape used by the UI). Settings are NOT offered here — they
 *    live on Settings → Plugin Settings.
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

test('Plugins settings page lists built-in plugins; details modal shows permissions', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const token = await authenticate(request)
  await loadAppAt(page, token, '/plugins')
  // The /plugins deep-link opens Settings → Plugins; the plugins section
  // renders inside the settings page.
  const section = page.getByTestId('plugins-section')
  await expect(section).toBeVisible({ timeout: 10_000 })

  // Two built-in plugins are registered (`claude-code` and `mock`); the
  // UI renders each as a compact row carrying its display name and an
  // "Active" status badge.
  const claudeRow = page.getByTestId('plugin-card-claude-code')
  await expect(claudeRow).toBeVisible()
  await expect(claudeRow).toContainText('Claude Code')
  await expect(claudeRow.locator('.plugin-badge--active')).toBeVisible()

  const mockRow = page.getByTestId('plugin-card-mock')
  await expect(mockRow).toBeVisible()
  await expect(mockRow).toContainText('Mock Provider')

  // Clicking the row opens the details modal: built-in tag plus the
  // requested permissions. The display labels come from the backend
  // Permission::label table; pinning a couple catches regressions in the
  // wire shape.
  await claudeRow.getByTestId('plugin-open-claude-code').click()
  const claudeDetails = page.getByTestId('plugin-details-claude-code')
  await expect(claudeDetails).toBeVisible()
  await expect(claudeDetails).toContainText('Built-in · always enabled')
  await expect(claudeDetails.locator('[data-permission="register_provider"]')).toBeVisible()
  await expect(claudeDetails.locator('[data-permission="spawn_process"]')).toBeVisible()
  await expect(claudeDetails.locator('[data-permission="network_access"]')).toBeVisible()
  // Plugin settings moved to Settings → Plugin Settings; the modal must
  // not offer them.
  await expect(claudeDetails.getByRole('button', { name: 'Settings' })).toHaveCount(0)
  await page.keyboard.press('Escape')
  await expect(claudeDetails).toHaveCount(0)

  // The mock plugin should declare exactly one permission — it does no
  // I/O. Asserting both the present permission and the absence of an
  // overreach (`spawn_process`) catches a future plugin author adding a
  // permission without realising the catalog displays it.
  await mockRow.getByTestId('plugin-open-mock').click()
  const mockDetails = page.getByTestId('plugin-details-mock')
  await expect(mockDetails).toBeVisible()
  await expect(mockDetails).toContainText('Built-in · always enabled')
  await expect(mockDetails.locator('[data-permission="register_provider"]')).toBeVisible()
  await expect(mockDetails.locator('[data-permission="spawn_process"]')).toHaveCount(0)
})
