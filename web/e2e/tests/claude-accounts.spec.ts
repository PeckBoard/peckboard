import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * UI e2e for multi-account Claude support (Settings → Claude Accounts).
 *
 * Two flows:
 *  1. The deterministic CRUD/picker/delete path, driven through an API-key
 *     account (its credential is a paste field, so no Anthropic round-trip is
 *     needed). Covers the masked hint, kind tag, warn badge, model-catalogue
 *     wiring, and delete.
 *  2. The browser "log in with Claude" flow for subscription accounts:
 *     generate a login URL, paste the code, and confirm the modal forwards the
 *     PKCE login to the server. The real OAuth exchange hits Anthropic, so the
 *     network calls are stubbed; the server-side exchange is unit-tested
 *     separately.
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

test('add, list, expose-in-picker, and delete a Claude account', async ({ request, page }) => {
  const token = await authenticate(request)
  await loadApp(page, token)

  // Baseline: no account-scoped models exist yet.
  expect(await accountModelLabels(request, token)).not.toContain('[E2E Work] Claude Opus 4.8')

  const settings = await openSettings(page)
  const section = settings.getByTestId('claude-accounts-section')
  await expect(section).toBeVisible()
  await expect(section).toContainText('No accounts added yet')

  // ── Add an API-key account (deterministic paste path) ──────────────
  await section.getByTestId('acct-add').click()
  const modal = page.getByTestId('claude-account-modal')
  await expect(modal).toBeVisible()

  await modal.getByTestId('acct-name').fill('E2E Work')
  await modal.getByTestId('acct-kind-api_key').click()
  await modal.getByTestId('acct-credential').fill('sk-ant-e2e-TESTTOKEN9999')
  // Give it a budget so the warn badge path renders (zero spend → "OK").
  await modal.getByTestId('acct-window').selectOption('24')
  await modal.getByTestId('acct-limit-tokens').fill('1000000')
  await modal.getByTestId('acct-save').click()
  await expect(modal).toBeHidden()

  // ── Row renders with masked hint, kind tag, and an OK warn badge ───
  const row = section.locator('[data-testid^="acct-row-"]')
  await expect(row).toHaveCount(1)
  await expect(row).toContainText('E2E Work')
  await expect(row).toContainText('API key')
  await expect(row).toContainText('••••9999') // masked credential
  const badge = section.locator('[data-testid^="acct-badge-"]')
  await expect(badge).toHaveAttribute('data-level', 'ok')

  // ── Account switching is wired: it appears in the model catalogue ──
  await expect
    .poll(() => accountModelLabels(request, token))
    .toContain('[E2E Work] Claude Opus 4.8')

  // ── Delete removes the row and drops it from the catalogue ─────────
  await row.locator('[data-testid^="acct-delete-"]').click()
  const confirm = page.locator('.confirm-dialog')
  await expect(confirm).toBeVisible()
  await confirm.getByRole('button', { name: 'Delete' }).click()

  await expect(section.locator('[data-testid^="acct-row-"]')).toHaveCount(0)
  await expect
    .poll(() => accountModelLabels(request, token))
    .not.toContain('[E2E Work] Claude Opus 4.8')
})

test('browser login flow: generate URL, paste code, forward the PKCE login', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await loadApp(page, token)

  // Stub the login-start so no real Claude round-trip is needed.
  await page.route('**/api/claude-accounts/login/start', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        url: 'https://claude.com/cai/oauth/authorize?code=true&state=STATE123',
        verifier: 'VERIFIER123',
        state: 'STATE123',
      }),
    })
  })

  // Stub the create POST (its server side would otherwise hit Anthropic's
  // token endpoint) and capture the body to confirm the modal forwards the
  // pasted login. GET (list) falls through to the real server.
  let createdBody: { kind?: string; login?: unknown } | null = null
  await page.route('**/api/claude-accounts', async (route) => {
    if (route.request().method() === 'POST') {
      createdBody = route.request().postDataJSON()
      await route.fulfill({ status: 201, contentType: 'application/json', body: '{}' })
    } else {
      await route.continue()
    }
  })

  const settings = await openSettings(page)
  const section = settings.getByTestId('claude-accounts-section')
  await section.getByTestId('acct-add').click()
  const modal = page.getByTestId('claude-account-modal')
  await expect(modal).toBeVisible()

  await modal.getByTestId('acct-name').fill('E2E Sub')
  // `oauth_token` (Subscription) is the default kind — generate the login URL.
  await modal.getByTestId('acct-login-start').click()
  await expect(modal.getByTestId('acct-login-url')).toBeVisible()
  await expect(modal.getByTestId('acct-login-url')).toHaveAttribute(
    'href',
    /claude\.com\/cai\/oauth\/authorize/,
  )

  // Paste the `code#state` Claude shows and save.
  await modal.getByTestId('acct-login-code').fill('AUTHCODE#STATE123')
  await modal.getByTestId('acct-save').click()
  await expect(modal).toBeHidden()

  expect(createdBody).not.toBeNull()
  expect(createdBody!.kind).toBe('oauth_token')
  expect(createdBody!.login).toEqual({
    code: 'AUTHCODE#STATE123',
    verifier: 'VERIFIER123',
    state: 'STATE123',
  })

  // Make sure no real account leaked in from the stubbed create.
  await page.unroute('**/api/claude-accounts')
  expect(await accountModelLabels(request, token)).not.toContain('[E2E Sub] Claude Opus 4.8')
})
