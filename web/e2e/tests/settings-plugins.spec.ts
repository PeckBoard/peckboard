import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * UI e2e for the Plugins settings sub-page (Settings → Plugins).
 *
 * The Plugins sub-page renders directly from `GET /api/plugins`, so this
 * is the user-visible counterpart to `tests/plugins_endpoint.rs`. We
 * verify:
 *
 * 1. The `/plugins` deep-link opens Settings → Plugins.
 * 2. The two built-in plugins (`claude-code`, `mock`) show up with their
 *    display names, "Built-in · always enabled" tag, and Active badge.
 * 3. Each plugin lists the permissions it was granted (the wire shape
 *    used by the UI).
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

test('Plugins settings page lists built-in plugins with their permissions', async ({
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
  await expect(section).toBeVisible({ timeout: 10_000 })

  // Two built-in plugins are registered (`claude-code` and `mock`); the
  // UI renders each as a card carrying its display name and an "Active"
  // status badge.
  const claudeCard = page.getByTestId('plugin-card-claude-code')
  await expect(claudeCard).toBeVisible()
  await expect(claudeCard).toContainText('Claude Code')
  await expect(claudeCard).toContainText('Built-in · always enabled')
  await expect(claudeCard.locator('.plugin-badge--active')).toBeVisible()

  const mockCard = page.getByTestId('plugin-card-mock')
  await expect(mockCard).toBeVisible()
  await expect(mockCard).toContainText('Mock Provider')
  await expect(mockCard).toContainText('Built-in · always enabled')

  // Each plugin renders its requested permissions. The display labels
  // come from the backend Permission::label table; pinning a couple
  // catches regressions in the wire shape.
  await expect(claudeCard.locator('[data-permission="register_provider"]')).toBeVisible()
  await expect(claudeCard.locator('[data-permission="spawn_process"]')).toBeVisible()
  await expect(claudeCard.locator('[data-permission="network_access"]')).toBeVisible()

  // The mock plugin should declare exactly one permission — it does no
  // I/O. Asserting both the present permission and the absence of an
  // overreach (`spawn_process`) catches a future plugin author adding a
  // permission without realising the catalog displays it.
  await expect(mockCard.locator('[data-permission="register_provider"]')).toBeVisible()
  await expect(mockCard.locator('[data-permission="spawn_process"]')).toHaveCount(0)
})
