import { test, expect, type APIRequestContext } from '@playwright/test'

/**
 * One plugin page handing the user off to another plugin's page.
 *
 * Graphify's "Install from App Manager" button is the real caller: it needs
 * to open the App Manager page with `?install=…` prefilled. A `target=_blank`
 * link cannot do it — plugin iframes are sandboxed without
 * `allow-same-origin`, a popup INHERITS that sandbox, and the opaque origin it
 * lands in makes the app's own asset requests carry `Origin: null`, which
 * `origin_check` (src/security.rs) answers with 403. So the page posts
 * `plugin-ui-open-page` and the host navigates in-app instead.
 *
 * Plugin pages are served by WASM plugins this repo can't compile in CI, so
 * the catalog and both plugin pages are mocked (same approach as
 * plugin-ui-panel.spec.ts); what's under test is the host plumbing.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const SOURCE_PATH = '/plugin-api/v1/source'
const TARGET_PATH = '/plugin-api/v1/target'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

test('a plugin page opens another plugin page in-app, carrying its query', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)

  await page.route('**/api/plugins', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        plugins: [],
        wasm_plugins: [],
        ui_panels: [],
        sidebar_items: [
          { plugin: 'source', id: 'source', label: 'Source', path: SOURCE_PATH },
          { plugin: 'target', id: 'target', label: 'Target', path: TARGET_PATH },
        ],
      }),
    })
  })

  // The asking page: one button posts a valid handoff, one posts a message
  // with an illegal plugin id that the host must drop.
  await page.route(`**${SOURCE_PATH}*`, async (route) => {
    await route.fulfill({
      contentType: 'text/html',
      body: `<!doctype html><html><body>
        <button data-testid="ask" onclick="window.parent.postMessage(
          { type: 'plugin-ui-open-page', plugin: 'target', item: 'target', query: 'install=pip&from=source' }, '*')">go</button>
        <button data-testid="ask-bad" onclick="window.parent.postMessage(
          { type: 'plugin-ui-open-page', plugin: '../etc', item: 'target' }, '*')">bad</button>
      </body></html>`,
    })
  })

  // The target page echoes the query it was served with, proving the deep
  // link reached the plugin and not just the address bar.
  await page.route(`**${TARGET_PATH}*`, async (route) => {
    const query = new URL(route.request().url()).search
    await route.fulfill({
      contentType: 'text/html',
      body: `<!doctype html><html><body><p data-testid="target-query">${query}</p></body></html>`,
    })
  })

  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/plugin-page/source/source')

  const source = page.frameLocator('[data-testid="plugin-fullpage-frame"][data-plugin="source"]')
  await expect(source.getByTestId('ask')).toBeVisible({ timeout: 10_000 })

  // A malformed message changes nothing.
  await source.getByTestId('ask-bad').click()
  await expect(page).toHaveURL(/\/plugin-page\/source\/source$/)

  await source.getByTestId('ask').click()

  // The app navigated: the URL carries the deep link, and the target plugin's
  // page was served with the query the asking page sent.
  await expect(page).toHaveURL(/\/plugin-page\/target\/target\?install=pip&from=source$/)
  const target = page.frameLocator('[data-testid="plugin-fullpage-frame"][data-plugin="target"]')
  await expect(target.getByTestId('target-query')).toContainText('install=pip')
  await expect(target.getByTestId('target-query')).toContainText('from=source')

  // Back returns to the asking page, forward restores the deep link.
  await page.goBack()
  await expect(page).toHaveURL(/\/plugin-page\/source\/source$/)
  await page.goForward()
  await expect(page).toHaveURL(/\/plugin-page\/target\/target\?install=pip&from=source$/)
  await expect(target.getByTestId('target-query')).toContainText('install=pip')
})
