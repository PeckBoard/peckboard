import { test, expect, type APIRequestContext } from '@playwright/test'

/**
 * UI e2e for the generic plugin UI-panel host (Settings → Plugins).
 *
 * A plugin declares a `ui_panel` in its manifest; core surfaces it in the
 * `GET /api/plugins` catalog as `{ plugin, id, title, path }`, and the
 * plugin's details modal lists it. Opening a panel embeds the
 * plugin-served page in a sandboxed `<iframe>` pointed at the panel's
 * `/plugin-api/*` path.
 *
 * The panel's page is served by a WASM plugin's own `/plugin-api` route,
 * which this repo can't compile in CI (no wasm32 toolchain — see
 * `tests/plugin_http_routes.rs`). So this test drives the host plumbing
 * deterministically by mocking the catalog (to declare a panel) and the
 * panel page (the bytes the plugin would serve). The backend surfacing of
 * `ui_panels` and the `/plugin-api/`-prefix path validation are covered by
 * `tests/plugins_endpoint.rs` and the `src/plugin/manager.rs` unit tests.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const PANEL_PATH = '/plugin-api/v1/demo-admin'

// A hostile plugin icon: one legitimate glyph path plus script /
// foreignObject / event-handler payloads. The host's sanitizer must keep
// the path and strip everything else without executing anything.
const EVIL_ICON =
  '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor">' +
  '<script>window.__pwned = true</script>' +
  '<path d="M3 12h18" onload="window.__pwned = true"/>' +
  '<foreignObject><img src="x" onerror="window.__pwned = true"/></foreignObject>' +
  '</svg>'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

test('plugin UI panel opens its plugin-served page in a sandboxed iframe', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)

  // Mock the catalog so an approved plugin declares one UI panel. Panels
  // render inside the details modal of the plugin that registered them,
  // so the catalog must report that plugin too.
  await page.route('**/api/plugins', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        plugins: [],
        wasm_plugins: [
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
        ],
        ui_panels: [{ plugin: 'demo', id: 'admin', title: 'Demo Admin', path: PANEL_PATH }],
        sidebar_items: [
          { plugin: 'demo', id: 'admin', label: 'Demo Rail', icon: EVIL_ICON, path: PANEL_PATH },
          { plugin: 'demo', id: 'plain', label: 'Plain Rail', path: PANEL_PATH },
        ],
      }),
    })
  })

  // Mock the plugin-served page the iframe loads (what the plugin's own
  // /plugin-api route would return).
  await page.route(`**${PANEL_PATH}`, async (route) => {
    await route.fulfill({
      contentType: 'text/html',
      body: '<!doctype html><html><body><h1 data-testid="panel-page-body">Plugin admin page loaded</h1></body></html>',
    })
  })

  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  // The /plugins deep-link opens Settings → Plugins.
  await page.goto('/plugins')

  await expect(page.getByTestId('plugins-section')).toBeVisible({ timeout: 10_000 })

  // The plugin's declared sidebar_item renders as a button in the left rail
  // (generic; opens the plugin page in the same iframe panel when clicked).
  await expect(page.getByTestId('plugin-sidebar-demo-admin')).toBeVisible()

  // The declared icon renders inline after strict sanitization: the glyph
  // path survives; script / foreignObject / event handlers do not, and none
  // of the payloads executed.
  const railBtn = page.getByTestId('plugin-sidebar-demo-admin')
  await expect(railBtn.locator('.plugin-icon svg path')).toHaveCount(1)
  const iconHtml = await railBtn.innerHTML()
  expect(iconHtml).not.toContain('script')
  expect(iconHtml).not.toContain('onload')
  expect(iconHtml).not.toContain('onerror')
  expect(iconHtml).not.toContain('foreignObject')
  expect(await page.evaluate(() => '__pwned' in window)).toBe(false)

  // An entry that declares no icon keeps the generic fallback glyph.
  const plainBtn = page.getByTestId('plugin-sidebar-demo-plain')
  await expect(plainBtn.locator('svg')).toBeVisible()
  await expect(plainBtn.locator('.plugin-icon')).toHaveCount(0)

  // The compact row shows the plugin's name and one-line summary; the full
  // manifest (version, source repo, contributed pages) lives in the row's
  // details modal — not a separate "Plugin Pages" section.
  const row = page.getByTestId('wasm-plugin-demo')
  await expect(row).toBeVisible()
  await expect(row).toContainText('Demo plugin')
  await row.getByTestId('wasm-plugin-open-demo').click()
  const details = page.getByTestId('plugin-details-demo')
  await expect(details).toBeVisible()
  await expect(details).toContainText('v1.0.0')
  await expect(details.getByRole('link', { name: 'github.com/acme/demo' })).toHaveAttribute(
    'href',
    'https://github.com/acme/demo',
  )
  await expect(details.getByTestId('plugin-panels')).toContainText('Demo Admin')
  await expect(page.getByText('Plugin Pages')).toHaveCount(0)

  // Opening it renders a sandboxed iframe pointed at the panel path.
  await details.getByTestId('plugin-panel-open-demo-admin').click()
  await expect(page.getByTestId('plugin-panel-modal')).toBeVisible()

  const frameEl = page.getByTestId('plugin-panel-frame')
  await expect(frameEl).toHaveAttribute('src', PANEL_PATH)
  // Sandboxed WITHOUT allow-same-origin: the plugin page can't reach the
  // host app's session token. This is the trust boundary for embedded
  // plugin pages — pin it so it can't silently regress.
  await expect(frameEl).toHaveAttribute('sandbox', 'allow-scripts allow-forms allow-popups')

  // The plugin-served page actually loads inside the iframe.
  const frame = page.frameLocator('[data-testid="plugin-panel-frame"]')
  await expect(frame.getByTestId('panel-page-body')).toContainText('Plugin admin page loaded')
})
