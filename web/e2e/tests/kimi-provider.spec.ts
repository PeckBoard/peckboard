import { test, expect, type APIRequestContext, type Page } from '../harness'

/**
 * Kimi Code (Moonshot AI) provider registration surface.
 *
 * The kimi builtin registers at startup, so with no CLI configured the
 * catalog must still show the provider, a Settings → Providers visibility
 * toggle, and its plugin entry with the settings schema. Kimi models are
 * account-scoped only (`kimi:<model>@<acct>`): with no account the
 * catalog lists no kimi model at all, and never a bare `kimi:<model>`.
 * Real turns need a signed-in `kimi` CLI and are not exercised here.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

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

test('kimi provider is registered with its config-default model and settings surface', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)

  // /api/models: provider present, no bare (non-account) kimi model.
  const modelsRes = await request.get('/api/models', { headers: authHeader })
  expect(modelsRes.ok()).toBeTruthy()
  const models = (await modelsRes.json()) as {
    providers: Array<{ id: string; display_name: string; models: Array<{ id: string }> }>
    models: Array<{ id: string }>
  }
  const kimi = models.providers.find((p) => p.id === 'kimi')
  expect(kimi, 'kimi provider missing from /api/models').toBeTruthy()
  expect(kimi!.display_name).toBe('Kimi Code')
  const bare = models.models.filter((m) => m.id.startsWith('kimi:') && !m.id.includes('@'))
  expect(bare.map((m) => m.id)).toEqual([])

  // /api/plugins: builtin entry carries the settings schema keys.
  const pluginsRes = await request.get('/api/plugins', { headers: authHeader })
  expect(pluginsRes.ok()).toBeTruthy()
  const plugins = (await pluginsRes.json()) as {
    plugins: Array<{ id: string; settings_schema: { fields: Array<{ key: string }> } }>
  }
  const plugin = plugins.plugins.find((p) => p.id === 'kimi')
  expect(plugin, 'kimi plugin missing from /api/plugins').toBeTruthy()
  const keys = plugin!.settings_schema.fields.map((f) => f.key)
  for (const key of ['cli_path', 'default_model', 'api_key', 'additional_models']) {
    expect(keys, `kimi settings schema missing ${key}`).toContain(key)
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
  const toggle = settingsPage.getByTestId('provider-toggle-kimi')
  await expect(toggle).toBeVisible()
  await expect(toggle).toBeChecked()
})
