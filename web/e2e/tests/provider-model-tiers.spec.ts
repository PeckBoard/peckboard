import { test, expect, type APIRequestContext, type Page } from '../harness'

/**
 * Settings → Providers & Accounts → Model Tiers.
 *
 * Uses `openai-compat`: its Additional Models setting gives it a known catalog
 * without any external server, and the Mock provider has no settings section.
 * Auto's use of these tiers is covered by the Rust tests, since Auto never
 * routes to `mock` and no real provider can run here.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const PROVIDER = 'openai-compat'
const MODEL_A = 'tier-test/model-a'
const MODEL_B = 'tier-test/model-b'

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

async function setCatalog(request: APIRequestContext, token: string, models: string[]) {
  const res = await request.put(`/api/plugins/${PROVIDER}/settings`, {
    headers: { Authorization: `Bearer ${token}` },
    data: { updates: { additional_models: models } },
  })
  expect(res.ok(), `settings failed: ${await res.text()}`).toBeTruthy()
}

async function modelTier(request: APIRequestContext, token: string, model: string) {
  const res = await request.get('/api/models', {
    headers: { Authorization: `Bearer ${token}` },
  })
  const body = (await res.json()) as { models: { id: string; tier?: number }[] }
  return body.models.find((m) => m.id === `${PROVIDER}:${model}`)?.tier
}

async function assign(page: Page, tier: number, model: string) {
  const picker = page.getByTestId(`provider-tier-add-${PROVIDER}-${tier}`)
  await picker.click()
  await page.getByTestId(`provider-tier-add-${PROVIDER}-${tier}-option-${model}`).click()
  await expect(
    page
      .getByTestId(`provider-tier-${PROVIDER}-${tier}`)
      .getByTestId(`provider-tier-chip-${model}`),
  ).toBeVisible()
}

// Specs in a shard share one server, so leave no models or tiers behind.
test.afterEach(async ({ request }) => {
  const token = await authenticate(request)
  await request.put(`/api/settings/provider-tiers/${PROVIDER}`, {
    headers: { Authorization: `Bearer ${token}` },
    data: { tiers: [] },
  })
  await setCatalog(request, token, [])
})

test('assigning models to tiers changes the catalog, persists, and resets', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await setCatalog(request, token, [MODEL_A, MODEL_B])
  await expect.poll(() => modelTier(request, token, MODEL_B), { timeout: 20_000 }).toBe(0)
  // Stubbed: which providers have models depends on what this box has installed.
  await page.route('**/api/settings/provider-tiers/ollama', (route) =>
    route.fulfill({
      json: {
        provider: 'ollama',
        display_name: 'Ollama',
        tiers: [{ tier: 1, label: 'Low', models: [] }],
        catalog: [],
      },
    }),
  )
  await loadAppAt(page, token, '/settings/providers')

  const tiers = page.getByTestId(`provider-tiers-${PROVIDER}`)
  await expect(tiers).toBeVisible({ timeout: 20_000 })
  const reset = page.getByTestId(`provider-tiers-reset-${PROVIDER}`)
  await expect(reset).toBeDisabled()

  // A provider with no models offers no empty rows.
  await expect(page.getByTestId('provider-tiers-ollama')).toContainText('no models right now', {
    timeout: 20_000,
  })
  await expect(page.getByTestId('provider-tier-add-ollama-1')).toHaveCount(0)

  // Assigning changes the tier every consumer of the catalog sees.
  await assign(page, 4, MODEL_A)
  await expect(page.getByTestId(`provider-tiers-customized-${PROVIDER}`)).toBeVisible()
  await expect.poll(() => modelTier(request, token, MODEL_A)).toBe(4)

  // The same model can also serve another tier; its catalog tier stays the highest.
  await page.getByTestId(`provider-tier-add-${PROVIDER}-4`).click()
  await expect(page.getByTestId(`provider-tier-add-${PROVIDER}-4-option-${MODEL_A}`)).toHaveCount(0)
  await page.keyboard.press('Escape')
  await assign(page, 1, MODEL_A)
  await expect(
    page.getByTestId(`provider-tier-${PROVIDER}-4`).getByTestId(`provider-tier-chip-${MODEL_A}`),
  ).toBeVisible()
  await expect.poll(() => modelTier(request, token, MODEL_A)).toBe(4)

  // Several models in one tier keep the order the user sets.
  await assign(page, 4, MODEL_B)
  const row = page.getByTestId(`provider-tier-${PROVIDER}-4`)
  await row.getByRole('button', { name: new RegExp(`Move ${MODEL_B}.* earlier`) }).click()
  const order = async () => {
    const res = await request.get(`/api/settings/provider-tiers/${PROVIDER}`, {
      headers: { Authorization: `Bearer ${token}` },
    })
    const body = (await res.json()) as { tiers: { tier: number; models: string[] }[] }
    return body.tiers.find((t) => t.tier === 4)?.models
  }
  await expect.poll(order).toEqual([MODEL_B, MODEL_A])

  // Survives a reload, order included.
  await page.reload()
  const chips = page
    .getByTestId(`provider-tier-${PROVIDER}-4`)
    .locator('[data-testid^="provider-tier-chip-"]')
  await expect(chips).toHaveCount(2, { timeout: 20_000 })
  await expect(chips.first()).toHaveAttribute('data-testid', `provider-tier-chip-${MODEL_B}`)

  // Reset asks first, then restores the provider's own tier.
  await page.getByTestId(`provider-tiers-reset-${PROVIDER}`).click()
  await page.getByRole('button', { name: 'Reset to defaults' }).last().click()
  await expect(page.getByTestId(`provider-tiers-customized-${PROVIDER}`)).toHaveCount(0)
  await expect.poll(() => modelTier(request, token, MODEL_A)).toBe(0)
})

test('the tiers API refuses a model twice in one tier and keeps the saved state', async ({
  request,
}) => {
  const token = await authenticate(request)
  const headers = { Authorization: `Bearer ${token}` }
  const res = await request.put(`/api/settings/provider-tiers/${PROVIDER}`, {
    headers,
    data: { tiers: [{ tier: 1, models: [MODEL_A, MODEL_A] }] },
  })
  expect(res.status()).toBe(400)
  expect(((await res.json()) as { error: string }).error).toContain('more than once')

  const after = await request.get(`/api/settings/provider-tiers/${PROVIDER}`, { headers })
  const body = (await after.json()) as { tiers: { models: string[] }[] }
  expect(body.tiers.flatMap((t) => t.models)).toEqual([])
})
