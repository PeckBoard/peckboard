import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * UI e2e for the Settings page + Plugins modal reached through the
 * user-icon dropdown in the nav rail.
 *
 * The nav rail no longer carries a dedicated Settings icon — both
 * Settings and Plugins are reached from the avatar dropdown at the
 * bottom of the rail. This covers:
 *
 * 1. The Settings rail icon is gone (no button titled "Settings" on the
 *    rail).
 * 2. Clicking the avatar reveals a menu with Settings and Plugins
 *    options.
 * 3. Settings opens a full-page view (not a modal); its Back button
 *    returns to the underlying view. Plugins opens a modal; closing it
 *    returns to the underlying view.
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

test('user dropdown opens Settings page and Plugins modal; rail Settings icon is gone', async ({
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
  // Sanity: the page still carries the core sections.
  await expect(settingsPage).toContainText('User Info')
  await expect(settingsPage).toContainText('Theme')

  // Back returns to the underlying view and unmounts the page.
  await settingsPage.getByRole('button', { name: 'Back' }).click()
  await expect(settingsPage).toBeHidden()

  // Open the dropdown again and pick Plugins.
  await page.locator('.rail-avatar').click()
  await expect(menu).toBeVisible()
  await menu.getByRole('menuitem', { name: 'Plugins' }).click()

  const pluginsModal = page.getByTestId('plugins-modal')
  await expect(pluginsModal).toBeVisible()
  // The wrapped section continues to expose its testid so existing
  // plugin tests keep working.
  await expect(pluginsModal.getByTestId('plugins-section')).toBeVisible()

  await pluginsModal.getByRole('button', { name: 'Close' }).click()
  await expect(pluginsModal).toBeHidden()
})
