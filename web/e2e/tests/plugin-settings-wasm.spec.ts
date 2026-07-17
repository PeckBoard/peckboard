import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * UI e2e for the settings form of an installed WASM plugin.
 *
 * A WASM plugin (nginx-manager here) can declare operator settings in its
 * manifest; the catalog carries them as `wasm_plugins[].settings_schema`
 * and the plugin gets a section on Settings → Plugin Settings
 * (`/plugin-settings`) rendering the same shared form built-in plugins
 * use, backed by the same `/api/plugins/:id/settings` routes. The
 * installed-plugins list itself offers NO settings entry point.
 *
 * A real `.wasm` can't be compiled in CI (no wasm32 toolchain — see
 * `plugin-approval-prompt.spec.ts` for the same constraint), so the catalog
 * and settings endpoints are mocked; the backend side of the round trip is
 * covered by `tests/plugins_endpoint.rs::wasm_plugin_settings_round_trip_via_routes`
 * against the actually-built wasm.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const SCHEMA = {
  fields: [
    {
      key: 'base_url',
      title: 'Nginx Proxy Manager URL',
      description: 'Root URL of the NPM admin interface.',
      required: true,
      type: 'url',
      placeholder: 'http://192.168.1.10:81',
    },
    {
      key: 'api_key',
      title: 'API key',
      description: 'NPM API key used as the Bearer token.',
      required: true,
      type: 'string',
      secret: true,
      placeholder: 'npm_…',
    },
  ],
}

const EMPTY_SETTINGS = [
  { key: 'base_url', value: null, has_value: false, masked: false },
  { key: 'api_key', value: null, has_value: false, masked: true },
]

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

test('WASM plugin settings live on Plugin Settings and save through the shared form', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)

  // Catalog reports one approved WASM plugin that declares settings.
  await page.route('**/api/plugins', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        plugins: [],
        ui_panels: [],
        wasm_plugins: [
          {
            name: 'nginx-manager',
            description: 'Manage an Nginx Proxy Manager instance.',
            version: '0.2.0',
            repository: 'https://github.com/PeckBoard/nginx-manager',
            hooks: ['mcp.tool.invoke'],
            permissions: ['provide_mcp_tools', 'http_request'],
            status: 'approved',
            error: null,
            settings_schema: SCHEMA,
          },
        ],
      }),
    })
  })

  // Settings endpoint: GET serves the schema + empty values; PUT captures
  // the update and echoes the post-save wire shape (secret masked).
  let putBody: { updates?: Record<string, unknown> } | null = null
  await page.route('**/api/plugins/nginx-manager/settings', async (route) => {
    if (route.request().method() === 'PUT') {
      putBody = route.request().postDataJSON() as { updates?: Record<string, unknown> }
      await route.fulfill({
        contentType: 'application/json',
        body: JSON.stringify({
          plugin_id: 'nginx-manager',
          schema: SCHEMA,
          settings: [
            { key: 'base_url', value: 'http://npm.local:81', has_value: true, masked: false },
            { key: 'api_key', value: null, has_value: true, masked: true },
          ],
        }),
      })
      return
    }
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        plugin_id: 'nginx-manager',
        schema: SCHEMA,
        settings: EMPTY_SETTINGS,
      }),
    })
  })

  // The /plugin-settings deep-link opens Settings → Plugin Settings, which
  // lists a section per plugin that declares settings.
  await loadAppAt(page, token, '/plugin-settings')
  const section = page.getByTestId('plugin-settings-section')
  await expect(section).toBeVisible({ timeout: 10_000 })
  const entry = page.getByTestId('plugin-settings-entry-nginx-manager')
  await expect(entry).toBeVisible()
  await expect(entry).toContainText('nginx-manager')

  // The shared form renders the manifest-declared fields.
  const form = entry.getByTestId('plugin-settings-nginx-manager')
  await expect(form.locator('[data-field="base_url"]')).toBeVisible()
  await expect(form.locator('[data-field="api_key"]')).toBeVisible()
  await expect(form.locator('[data-field="base_url"]')).toContainText('Nginx Proxy Manager URL')
  await expect(form.locator('[data-field="api_key"]')).toContainText('API key')

  // Fill URL + token and save; the PUT carries exactly the two updates.
  await form.locator('[data-field="base_url"] input').fill('http://npm.local:81')
  await form.locator('[data-field="api_key"] input').fill('npm_e2e_secret_token')
  await page.screenshot({ path: 'e2e/test-results/wasm-plugin-settings-form.png' })
  await form.locator('.plugin-settings-save').click()
  await expect(form.locator('.plugin-settings-success')).toBeVisible({ timeout: 5_000 })

  expect(putBody).not.toBeNull()
  expect(putBody!.updates).toEqual({
    base_url: 'http://npm.local:81',
    api_key: 'npm_e2e_secret_token',
  })

  // After the save the secret input is empty again (never echoed) but the
  // form shows the "currently set" hint.
  await expect(form.locator('[data-field="api_key"] input')).toHaveValue('')
  await expect(form.locator('[data-field="api_key"] .plugin-setting-secret-set')).toBeVisible()

  // The installed-plugins list itself no longer offers a Settings entry
  // point — neither on the row nor in the details modal.
  await page.goto('/plugins')
  const row = page.getByTestId('wasm-plugin-nginx-manager')
  await expect(row).toBeVisible({ timeout: 10_000 })
  await expect(page.getByTestId('wasm-plugin-settings-nginx-manager')).toHaveCount(0)
  await row.getByTestId('wasm-plugin-open-nginx-manager').click()
  const details = page.getByTestId('plugin-details-nginx-manager')
  await expect(details).toBeVisible()
  await expect(details.getByRole('button', { name: 'Settings' })).toHaveCount(0)
})
