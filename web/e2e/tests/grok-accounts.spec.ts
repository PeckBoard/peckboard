import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * UI e2e for multi-account Grok support (Settings → Grok Accounts).
 *
 * Two flows:
 *  1. The deterministic CRUD/picker/delete path, driven through an API-key
 *     account (its credential is a paste field, so no xAI round-trip is
 *     needed). Covers the kind tag, warn badge, model-catalogue wiring, and
 *     delete.
 *  2. The browser device sign-in flow: add a device account, then confirm the
 *     sign-in modal surfaces the `accounts.x.ai/oauth2/device` link. The real
 *     login spawns the `grok` CLI, so `login/start` is stubbed; the URL
 *     scraper + login manager are unit-tested separately.
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

test('add, list, expose-in-picker, and delete a Grok api-key account', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await loadApp(page, token)

  // Baseline: no account-scoped models exist yet.
  expect(await accountModelLabels(request, token)).not.toContain('[E2E Grok] Grok Build')

  const settings = await openSettings(page)
  const section = settings.getByTestId('grok-accounts-section')
  await expect(section).toBeVisible()
  await expect(section).toContainText('No accounts added yet')

  // ── Add an API-key account (deterministic paste path) ──────────────
  await section.getByTestId('grok-acct-add').click()
  const modal = page.getByTestId('grok-account-modal')
  await expect(modal).toBeVisible()

  await modal.getByTestId('grok-acct-name').fill('E2E Grok')
  await modal.getByTestId('grok-acct-kind-api_key').click()
  await modal.getByTestId('grok-acct-credential').fill('xai-e2e-TESTKEY9999')
  // Give it a budget so the warn badge path renders (zero spend → "OK").
  await modal.getByTestId('grok-acct-window').selectOption('24')
  await modal.getByTestId('grok-acct-limit-tokens').fill('1000000')
  await modal.getByTestId('grok-acct-save').click()
  await expect(modal).toBeHidden()

  // ── Row renders with kind tag and an OK warn badge ─────────────────
  const row = section.locator('[data-testid^="grok-acct-row-"]')
  await expect(row).toHaveCount(1)
  await expect(row).toContainText('E2E Grok')
  await expect(row).toContainText('API key')
  const badge = section.locator('[data-testid^="grok-acct-badge-"]')
  await expect(badge).toHaveAttribute('data-level', 'ok')

  // ── Account switching is wired: it appears in the model catalogue ──
  await expect.poll(() => accountModelLabels(request, token)).toContain('[E2E Grok] Grok Build')

  // ── Delete removes the row and drops it from the catalogue ─────────
  await row.locator('[data-testid^="grok-acct-delete-"]').click()
  const confirm = page.locator('.confirm-dialog')
  await expect(confirm).toBeVisible()
  await confirm.getByRole('button', { name: 'Delete' }).click()

  await expect(section.locator('[data-testid^="grok-acct-row-"]')).toHaveCount(0)
  await expect.poll(() => accountModelLabels(request, token)).not.toContain('[E2E Grok] Grok Build')
})

test('device sign-in flow: add account then surface the grok device link', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await loadApp(page, token)

  // Stub login-start so no real `grok login` process is spawned. The path
  // carries the new account id, so match any account.
  await page.route('**/api/grok-accounts/*/login/start', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        url: 'https://accounts.x.ai/oauth2/device?user_code=TEST-1234',
      }),
    })
  })

  const settings = await openSettings(page)
  const section = settings.getByTestId('grok-accounts-section')
  await section.getByTestId('grok-acct-add').click()
  const modal = page.getByTestId('grok-account-modal')
  await expect(modal).toBeVisible()

  // `device` (Sign in) is the default kind — just name it and add.
  await modal.getByTestId('grok-acct-name').fill('E2E Grok Sub')
  await modal.getByTestId('grok-acct-save').click()
  await expect(modal).toBeHidden()

  // Creating a device account opens the sign-in modal; pressing "Sign in"
  // fetches and renders the device-login link from the stubbed login/start.
  const signIn = page.getByTestId('grok-signin-modal')
  await expect(signIn).toBeVisible()
  await signIn.getByTestId('grok-signin-start').click()
  const link = signIn.getByTestId('grok-signin-url')
  await expect(link).toBeVisible()
  await expect(link).toHaveAttribute('href', /accounts\.x\.ai\/oauth2\/device/)
  await expect(signIn.getByTestId('grok-signin-waiting')).toBeVisible()

  await signIn.getByTestId('grok-signin-close').click()

  // The account exists but reads as "Not signed in" (auth.json never written
  // because login was stubbed).
  const row = section.locator('[data-testid^="grok-acct-row-"]')
  await expect(row).toContainText('E2E Grok Sub')
  await expect(section.locator('[data-testid^="grok-acct-unauth-"]')).toBeVisible()
})
