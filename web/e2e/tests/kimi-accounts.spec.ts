import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * UI e2e for multi-account Kimi support (Settings → Kimi Accounts).
 *
 * Two flows, mirroring grok-accounts.spec.ts:
 *  1. The deterministic CRUD/picker/delete path, driven through an API-key
 *     account (its credential is a paste field, so no Moonshot round-trip is
 *     needed). Covers the kind tag, warn badge, model-catalogue wiring, and
 *     delete.
 *  2. The browser device sign-in flow: add a device account, then confirm the
 *     sign-in modal surfaces the `www.kimi.com/code/authorize_device` link.
 *     The real login spawns the `kimi` CLI, so `login/start` is stubbed; the
 *     URL scraper + login manager are unit-tested separately.
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

async function loadApp(page: Page, token: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/')
  await expect(page.locator('.rail-brand')).toBeVisible({ timeout: 10_000 })
}

async function openSettings(page: Page) {
  await page.locator('.rail-avatar').click()
  const menu = page.locator('.user-menu-dropdown')
  await expect(menu).toBeVisible()
  await menu.getByRole('menuitem', { name: 'Settings' }).click()
  const settings = page.getByTestId('settings-page')
  await expect(settings).toBeVisible()
  // Accounts live on the Providers & Accounts sub-page.
  await settings.getByTestId('settings-nav-providers').click()
  return settings
}

/** Account-scoped model display names served by `/api/models`. */
async function accountModelLabels(request: APIRequestContext, token: string): Promise<string[]> {
  const res = await request.get('/api/models', {
    headers: { Authorization: `Bearer ${token}` },
  })
  expect(res.ok()).toBeTruthy()
  const body = (await res.json()) as { models: { id: string; display_name: string }[] }
  return body.models.map((m) => m.display_name)
}

test('add, list, expose-in-picker, and delete a Kimi api-key account', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await loadApp(page, token)

  // Baseline: no account-scoped models exist yet.
  expect(await accountModelLabels(request, token)).not.toContain('[E2E Kimi] Default (Kimi config)')

  const settings = await openSettings(page)
  const section = settings.getByTestId('kimi-accounts-section')
  await expect(section).toBeVisible()
  await expect(section).toContainText('No accounts added yet')

  // ── Add an API-key account (deterministic paste path) ──────────────
  await section.getByTestId('kimi-acct-add').click()
  const modal = page.getByTestId('kimi-account-modal')
  await expect(modal).toBeVisible()

  await modal.getByTestId('kimi-acct-name').fill('E2E Kimi')
  await modal.getByTestId('kimi-acct-kind-api_key').click()
  await modal.getByTestId('kimi-acct-credential').fill('sk-e2e-TESTKEY9999')
  // Give it a budget so the warn badge path renders (zero spend → "OK").
  await modal.getByTestId('kimi-acct-window').selectOption('24')
  await modal.getByTestId('kimi-acct-limit-tokens').fill('1000000')
  await modal.getByTestId('kimi-acct-save').click()
  await expect(modal).toBeHidden()

  // ── Row renders with kind tag and an OK warn badge ─────────────────
  const row = section.locator('[data-testid^="kimi-acct-row-"]')
  await expect(row).toHaveCount(1)
  await expect(row).toContainText('E2E Kimi')
  await expect(row).toContainText('API key')
  const badge = section.locator('[data-testid^="kimi-acct-badge-"]')
  await expect(badge).toHaveAttribute('data-level', 'ok')

  // ── Account switching is wired: it appears in the model catalogue ──
  await expect
    .poll(() => accountModelLabels(request, token))
    .toContain('[E2E Kimi] Default (Kimi config)')

  // ── Delete removes the row and drops it from the catalogue ─────────
  await row.locator('[data-testid^="kimi-acct-delete-"]').click()
  const confirm = page.locator('.confirm-dialog')
  await expect(confirm).toBeVisible()
  await confirm.getByRole('button', { name: 'Delete' }).click()

  await expect(section.locator('[data-testid^="kimi-acct-row-"]')).toHaveCount(0)
  await expect
    .poll(() => accountModelLabels(request, token))
    .not.toContain('[E2E Kimi] Default (Kimi config)')
})

test('device sign-in flow: add account then surface the kimi device link', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await loadApp(page, token)

  // Stub login-start so no real `kimi login` process is spawned. The path
  // carries the new account id, so match any account.
  await page.route('**/api/kimi-accounts/*/login/start', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        url: 'https://www.kimi.com/code/authorize_device?user_code=TEST-1234',
      }),
    })
  })

  const settings = await openSettings(page)
  const section = settings.getByTestId('kimi-accounts-section')
  await section.getByTestId('kimi-acct-add').click()
  const modal = page.getByTestId('kimi-account-modal')
  await expect(modal).toBeVisible()

  // `device` (Sign in) is the default kind — just name it and add.
  await modal.getByTestId('kimi-acct-name').fill('E2E Kimi Sub')
  await modal.getByTestId('kimi-acct-save').click()
  await expect(modal).toBeHidden()

  // Creating a device account opens the sign-in modal; pressing "Sign in"
  // fetches and renders the device-login link from the stubbed login/start.
  const signIn = page.getByTestId('kimi-signin-modal')
  await expect(signIn).toBeVisible()
  await signIn.getByTestId('kimi-signin-start').click()
  const link = signIn.getByTestId('kimi-signin-url')
  await expect(link).toBeVisible()
  await expect(link).toHaveAttribute('href', /www\.kimi\.com\/code\/authorize_device/)
  await expect(signIn.getByTestId('kimi-signin-waiting')).toBeVisible()

  await signIn.getByTestId('kimi-signin-close').click()

  // The account exists but reads as "Not signed in" (its config.toml is
  // never written because login was stubbed).
  const row = section.locator('[data-testid^="kimi-acct-row-"]')
  await expect(row).toContainText('E2E Kimi Sub')
  await expect(section.locator('[data-testid^="kimi-acct-unauth-"]')).toBeVisible()
})
