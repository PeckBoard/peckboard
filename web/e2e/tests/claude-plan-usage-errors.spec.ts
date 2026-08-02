import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * Friendly plan-usage errors on Settings → Providers → Claude Accounts.
 *
 * The server's raw `last_error` (e.g. `usage fetch failed (429 Too Many
 * Requests): { "error": ... }`) must never render verbatim — the panel
 * shows a friendly message classified by error type, with the raw payload
 * tucked behind a `<details>`. Rate-limit errors additionally retry
 * automatically (no manual Refresh click needed).
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

async function navigateToProviders(page: Page) {
  await expect(page.locator('.rail-brand')).toBeVisible({ timeout: 10_000 })
  await page.locator('.rail-avatar').click()
  const menu = page.locator('.user-menu-dropdown')
  await expect(menu).toBeVisible()
  await menu.getByRole('menuitem', { name: 'Settings' }).click()
  const settingsPage = page.getByTestId('settings-page')
  await expect(settingsPage).toBeVisible()
  await settingsPage.getByTestId('settings-nav-providers').click()
  return settingsPage
}

const RATE_LIMIT_ERROR =
  'usage fetch failed (429 Too Many Requests): { "error": { "type": "rate_limit_error", "message": "Number of concurrent connections exceeds your limit" } }'

const AUTH_ERROR =
  'usage fetch failed (401 Unauthorized): { "error": { "type": "authentication_error" } }'

test('a rate-limited plan-usage error shows a friendly message and retries automatically', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)

  let refreshCount = 0
  await page.route('**/api/claude-accounts', (route) =>
    route.fulfill({ contentType: 'application/json', body: JSON.stringify([]) }),
  )
  await page.route('**/api/claude-accounts/plan-usage', (route) => {
    if (route.request().method() === 'POST') refreshCount++
    return route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        default: { usage: null, fetched_at: null, last_error: RATE_LIMIT_ERROR },
      }),
    })
  })

  await page.clock.install()
  await loadAppAt(page, token, '/')
  const settingsPage = await navigateToProviders(page)

  const panel = settingsPage.getByTestId('claude-plan-usage')
  await expect(panel).toBeVisible()
  const summary = panel.locator('.acct-plan-error > span').first()
  await expect(summary).toHaveText('Rate limited by the API — retrying automatically.')

  // Raw payload is present but tucked behind <details>, not shown by default.
  const details = panel.locator('.acct-plan-error-details')
  await expect(details.locator('pre')).toContainText(RATE_LIMIT_ERROR)
  await expect(details.locator('pre')).not.toBeVisible()

  expect(refreshCount).toBe(0)
  await page.clock.fastForward(31_000)
  await expect.poll(() => refreshCount).toBe(1)
  await page.clock.fastForward(61_000)
  await expect.poll(() => refreshCount).toBe(2)
})

test('an auth plan-usage error tells the user to re-add the account', async ({ request, page }) => {
  const token = await authenticate(request)

  await page.route('**/api/claude-accounts', (route) =>
    route.fulfill({ contentType: 'application/json', body: JSON.stringify([]) }),
  )
  await page.route('**/api/claude-accounts/plan-usage', (route) =>
    route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        default: { usage: null, fetched_at: null, last_error: AUTH_ERROR },
      }),
    }),
  )

  await loadAppAt(page, token, '/')
  const settingsPage = await navigateToProviders(page)

  const panel = settingsPage.getByTestId('claude-plan-usage')
  await expect(panel).toBeVisible()
  const summary = panel.locator('.acct-plan-error > span').first()
  await expect(summary).toHaveText(
    'Authentication failed — re-add this account to refresh its credentials.',
  )

  const details = panel.locator('.acct-plan-error-details')
  await expect(details.locator('pre')).toContainText(AUTH_ERROR)
  await expect(details.locator('pre')).not.toBeVisible()
})
