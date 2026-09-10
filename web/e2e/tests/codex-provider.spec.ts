import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * Codex (CLI) provider registration surface.
 *
 * The codex builtin registers at startup, so with no CLI configured the
 * catalog must still show the provider with its seed models
 * (`codex:gpt-5.6-sol` etc.; discovery falls back to the seed), a
 * Settings → Providers visibility toggle, and its plugin entry with the
 * settings schema. Real turns need a signed-in `codex` CLI and are not
 * exercised here — see the live smoke on the provider e2e card.
 *
 * Missing-CLI install-prompt UX lives in `codex-cli-install.spec.ts`.
 *
 * Assert slugs present in both the static seed (`default_models`) and the
 * live bundled catalog (`codex debug models --bundled`). Skip
 * `codex:gpt-5.6`: discovery replaces the seed when the CLI is on PATH,
 * and the bundled list does not include that id.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const SEED_MODELS = [
  'codex:gpt-5.6-luna',
  'codex:gpt-5.6-terra',
  'codex:gpt-5.6-sol',
  'codex:gpt-6-astra',
]

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

test('codex provider is registered with seed models and settings surface', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)

  // /api/models: provider present with the seed models (or the live bundled
  // catalog when `codex` is on PATH — those still include the seed slugs).
  const modelsRes = await request.get('/api/models', { headers: authHeader })
  expect(modelsRes.ok()).toBeTruthy()
  const models = (await modelsRes.json()) as {
    providers: Array<{ id: string; display_name: string; models: Array<{ id: string }> }>
    models: Array<{ id: string }>
  }
  const codex = models.providers.find((p) => p.id === 'codex')
  expect(codex, 'codex provider missing from /api/models').toBeTruthy()
  expect(codex!.display_name).toBe('Codex (CLI)')
  const ids = models.models.map((m) => m.id)
  for (const id of SEED_MODELS) {
    expect(ids, `seed model ${id} missing from /api/models`).toContain(id)
  }

  // /api/plugins: builtin entry carries the settings schema keys.
  const pluginsRes = await request.get('/api/plugins', { headers: authHeader })
  expect(pluginsRes.ok()).toBeTruthy()
  const plugins = (await pluginsRes.json()) as {
    plugins: Array<{ id: string; settings_schema: { fields: Array<{ key: string }> } }>
  }
  const plugin = plugins.plugins.find((p) => p.id === 'codex')
  expect(plugin, 'codex plugin missing from /api/plugins').toBeTruthy()
  const keys = plugin!.settings_schema.fields.map((f) => f.key)
  for (const key of ['cli_path', 'discover_models', 'additional_models']) {
    expect(keys, `codex settings schema missing ${key}`).toContain(key)
  }

  // Settings → Providers: visibility toggle rendered and on by default.
  await loadAppAt(page, token, '/')
  await expect(page.locator('.rail-brand')).toBeVisible({ timeout: 10_000 })
  await page.locator('.rail-avatar').click()
  const menu = page.locator('.user-menu-dropdown')
  await expect(menu).toBeVisible()
  await menu.getByRole('menuitem', { name: 'Settings' }).click()
  const settingsPage = page.getByTestId('settings-page')
  await expect(settingsPage).toBeVisible()
  await settingsPage.getByTestId('settings-nav-providers').click()
  const toggle = settingsPage.getByTestId('provider-toggle-codex')
  await expect(toggle).toBeVisible()
  await expect(toggle).toBeChecked()
})
