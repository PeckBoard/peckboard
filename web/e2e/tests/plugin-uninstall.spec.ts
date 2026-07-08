import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * UI e2e for uninstalling an installed WASM plugin from the Plugins modal
 * (Settings → Plugins). Each installed plugin row carries a "Remove"
 * button; confirming it DELETEs `/api/plugins/:id`, which shuts the plugin
 * down, deletes its `.wasm`, and clears its approval + settings server-side.
 * Built-in plugins have no such control.
 *
 * A real `.wasm` round trip can't be compiled in CI (no wasm32 toolchain —
 * see `tests/plugin_http_routes.rs`), so this drives the host plumbing
 * deterministically by mocking the catalog (to report one installed plugin)
 * and the DELETE endpoint. Backend removal + state-clearing are covered by
 * `tests/plugin_approvals.rs` and the `src/plugin/manager.rs` unit tests.
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

test('removes an installed plugin from the Plugins modal', async ({ request, page, baseURL }) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)

  // Mutable catalog: one approved WASM plugin until it's uninstalled, then
  // none — so the refetch after DELETE drops the row.
  const state = { installed: true, deleted: null as string | null }

  await page.route('**/api/plugins', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        plugins: [],
        ui_panels: [],
        wasm_plugins: state.installed
          ? [
              {
                name: 'demo',
                description: 'Demo plugin',
                version: '1.0.0',
                repository: 'https://github.com/acme/demo',
                hooks: ['http.request.before'],
                permissions: [],
                status: 'approved',
                error: null,
              },
            ]
          : [],
      }),
    })
  })

  // The DELETE endpoint records the id and flips the catalog to empty.
  await page.route('**/api/plugins/demo', async (route) => {
    expect(route.request().method()).toBe('DELETE')
    state.deleted = 'demo'
    state.installed = false
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({ removed: 'demo' }),
    })
  })

  await loadAppAt(page, token, '/plugins')
  await expect(page.getByTestId('plugins-section')).toBeVisible({ timeout: 10_000 })

  // The installed plugin row is listed with a Remove control.
  const row = page.getByTestId('wasm-plugin-demo')
  await expect(row).toBeVisible()
  await page.getByTestId('wasm-plugin-remove-demo').click()

  // Confirming the destructive dialog fires the DELETE and the row drops out
  // once the catalog is refetched. Scope to the dialog so we don't match the
  // row's own "Remove" button.
  await page.locator('.confirm-dialog').getByRole('button', { name: 'Remove' }).click()
  await expect(page.getByTestId('wasm-plugin-demo')).toHaveCount(0)
  expect(state.deleted).toBe('demo')
})
