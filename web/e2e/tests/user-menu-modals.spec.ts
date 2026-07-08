import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * UI e2e for the Settings page reached through the user-icon dropdown
 * in the nav rail, including its Plugins sub-page.
 *
 * The nav rail no longer carries a dedicated Settings icon — Settings is
 * reached from the avatar dropdown at the bottom of the rail; Plugins is
 * a sub-page inside it. This covers:
 *
 * 1. The Settings rail icon is gone (no button titled "Settings" on the
 *    rail).
 * 2. Clicking the avatar reveals a menu with a Settings option (no
 *    dedicated Plugins entry).
 * 3. Settings opens a full-page view (not a modal); its Back button
 *    returns to the underlying view. The Plugins sub-page renders the
 *    plugins section; Back returns to the Settings hub.
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

test('user dropdown opens Settings page with a Plugins sub-page; rail Settings icon is gone', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)
  await loadAppAt(page, token, '/')

  // Wait for the rail to render (use the brand image as a stable anchor).
  await expect(page.locator('.rail-brand')).toBeVisible({ timeout: 10_000 })

  // The Settings cog icon must NOT exist on the rail — Settings now
  // lives behind the avatar dropdown.
  await expect(page.locator('.rail button[title="Settings"]')).toHaveCount(0)

  // Open the avatar dropdown and click Settings.
  await page.locator('.rail-avatar').click()
  const menu = page.locator('.user-menu-dropdown')
  await expect(menu).toBeVisible()
  await menu.getByRole('menuitem', { name: 'Settings' }).click()

  const settingsPage = page.getByTestId('settings-page')
  await expect(settingsPage).toBeVisible()
  // Settings is a full-page view at `/settings`.
  await expect(page).toHaveURL(/\/settings$/)
  // The hub view carries user info plus one nav card per sub-page.
  await expect(settingsPage).toContainText('User Info')
  await expect(settingsPage.getByTestId('settings-nav-appearance')).toBeVisible()
  await expect(settingsPage.getByTestId('settings-nav-server')).toBeVisible()

  // Provider Keep-Alive lives on the Providers & Accounts sub-page and
  // reports the cadence and per-login (per account per provider)
  // last-run status, sourced from GET /api/config.
  await settingsPage.getByTestId('settings-nav-providers').click()
  const keepAlive = settingsPage.getByTestId('keepalive-section')
  await expect(keepAlive).toContainText('Provider Keep-Alive')
  await expect(keepAlive).toContainText(/Runs every hour|Runs every \d+ hours|disabled/)

  // Back first returns to the hub, then to the underlying view.
  await settingsPage.getByRole('button', { name: 'Back' }).click()
  await expect(settingsPage.getByTestId('settings-nav-providers')).toBeVisible()
  await settingsPage.getByRole('button', { name: 'Back' }).click()
  await expect(settingsPage).toBeHidden()

  // Open the dropdown again, go to Settings, and open the Plugins sub-page.
  await page.locator('.rail-avatar').click()
  await expect(menu).toBeVisible()
  await menu.getByRole('menuitem', { name: 'Settings' }).click()
  await expect(settingsPage).toBeVisible()

  await settingsPage.getByTestId('settings-nav-plugins').click()
  await expect(settingsPage.getByTestId('plugins-section')).toBeVisible()

  // Back returns to the Settings hub.
  await settingsPage.getByRole('button', { name: 'Back' }).click()
  await expect(settingsPage.getByTestId('settings-nav-plugins')).toBeVisible()
})
