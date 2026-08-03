/**
 * e2e tests for the Settings shell after the grouped-sidebar overhaul:
 *
 * 1. Desktop `/settings` lands on the Account page (no empty hub pane) and
 *    the rail shows group titles.
 * 2. Sub-page navigation is URL-synced (`/settings/<id>`), so browser
 *    back/forward walk the visited pages and a deep link restores one.
 * 3. Legacy entry URLs (`/plugins`, `/users`) keep working and
 *    canonicalize to their `/settings/<id>` address.
 * 4. The sidebar search filters pages, surfaces section-level hits that
 *    jump-and-highlight, and shows an empty state for a miss.
 */

import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

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

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

test('desktop /settings lands on Account under a grouped rail', async ({ request, page }) => {
  const token = await authenticate(request)
  await loadAt(page, token, '/settings')

  const settings = page.getByTestId('settings-page')
  await expect(settings).toBeVisible({ timeout: 10_000 })
  await expect(settings).toHaveAttribute('data-sub', 'account')
  await expect(settings).toContainText('User Info')
  // Landing does not rewrite the bare /settings URL.
  await expect(page).toHaveURL(/\/settings$/)

  // Group titles render in the rail (admin sees all of them).
  const nav = settings.getByRole('navigation', { name: 'Settings sections' })
  await expect(nav).toContainText('General')
  await expect(nav).toContainText('Agents')
  await expect(nav).toContainText('Connections')
  await expect(nav).toContainText('Plugins')
  await expect(nav).toContainText('Administration')
})

test('sub-page navigation is URL-synced and survives back/forward/reload', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await loadAt(page, token, '/settings')
  const settings = page.getByTestId('settings-page')
  await expect(settings).toBeVisible({ timeout: 10_000 })

  await settings.getByTestId('settings-nav-providers').click()
  await expect(page).toHaveURL(/\/settings\/providers$/)
  await settings.getByTestId('settings-nav-chat').click()
  await expect(page).toHaveURL(/\/settings\/chat$/)
  await expect(settings.getByTestId('caveman-section')).toBeVisible()

  // Browser back walks to the previously open sub-page.
  await page.goBack()
  await expect(page).toHaveURL(/\/settings\/providers$/)
  await expect(page.getByTestId('settings-page')).toHaveAttribute('data-sub', 'providers')

  // Forward returns, and a reload restores the page from the URL alone.
  await page.goForward()
  await expect(page).toHaveURL(/\/settings\/chat$/)
  await page.reload()
  await expect(page.getByTestId('settings-page')).toHaveAttribute('data-sub', 'chat', {
    timeout: 10_000,
  })

  // A direct deep link opens an admin sub-page.
  await page.goto('/settings/security')
  await expect(page.getByTestId('settings-page')).toHaveAttribute('data-sub', 'security')
  await expect(page.getByTestId('claude-permissions-section')).toBeVisible()
})

test('legacy /plugins and /users URLs redirect into Settings and canonicalize', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)

  await loadAt(page, token, '/plugins')
  await expect(page.getByTestId('plugins-section')).toBeVisible({ timeout: 10_000 })
  await expect(page).toHaveURL(/\/settings\/plugins$/)

  await page.goto('/users')
  await expect(page.getByTestId('user-management')).toBeVisible({ timeout: 10_000 })
  await expect(page).toHaveURL(/\/settings\/users$/)
})

test('sidebar search filters pages, jumps to section hits, and reports misses', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await loadAt(page, token, '/settings')
  const settings = page.getByTestId('settings-page')
  await expect(settings).toBeVisible({ timeout: 10_000 })

  const search = settings.getByTestId('settings-search')
  await search.fill('backup')
  // Only the Data page survives the filter; other entries are gone.
  await expect(settings.getByTestId('settings-nav-data')).toBeVisible()
  await expect(settings.getByTestId('settings-nav-appearance')).toHaveCount(0)

  // The section-level hit jumps to the page and flashes the section.
  await settings.getByTestId('settings-search-hit-backup').click()
  await expect(settings).toHaveAttribute('data-sub', 'data')
  await expect(settings.getByTestId('backup-section')).toBeVisible()
  await expect(settings.locator('.settings-section-flash')).toHaveCount(1)
  // Jumping clears the query, restoring the full rail.
  await expect(search).toHaveValue('')
  await expect(settings.getByTestId('settings-nav-appearance')).toBeVisible()

  // A miss says so instead of rendering an empty rail silently.
  await search.fill('zzz-no-such-setting')
  await expect(settings.getByTestId('settings-search-empty')).toBeVisible()
  await search.press('Escape')
  await expect(settings.getByTestId('settings-search-empty')).toHaveCount(0)
})
