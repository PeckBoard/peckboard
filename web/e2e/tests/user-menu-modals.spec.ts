import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * UI e2e for the Settings + Plugins modals reached through the user-icon
 * dropdown in the nav rail.
 *
 * The nav rail no longer carries a dedicated Settings icon — both
 * Settings and Plugins are reached from the avatar dropdown at the
 * bottom of the rail. This covers:
 *
 * 1. The Settings rail icon is gone (no button titled "Settings" on the
 *    rail).
 * 2. Clicking the avatar reveals a menu with Settings and Plugins
 *    options.
 * 3. Each option opens the corresponding modal; closing returns to the
 *    underlying view.
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

test('user dropdown opens Settings and Plugins modals; rail Settings icon is gone', async ({
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

  const settingsModal = page.getByTestId('settings-modal')
  await expect(settingsModal).toBeVisible()
  // Sanity: the modal still carries the core sections.
  await expect(settingsModal).toContainText('User Info')
  await expect(settingsModal).toContainText('Theme')

  // Close — opens via Close button.
  await settingsModal.getByRole('button', { name: 'Close' }).click()
  await expect(settingsModal).toBeHidden()

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
