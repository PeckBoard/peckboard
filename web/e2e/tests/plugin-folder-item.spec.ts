import { test, expect, type APIRequestContext } from '../harness'

/**
 * UI e2e for folder-scoped plugin pages (manifest `folder_items`).
 *
 * A plugin holding `contribute_sidebar` declares `folder_items`; core surfaces
 * them in the `GET /api/plugins` catalog, and the Folders page offers one
 * menu item per entry on every registered folder row. Clicking opens the plugin's
 * `/plugin-api/*` page at `/folders/<folderId>/plugin/<itemId>`, and the page's
 * authed `/api/plugin-ui/*` calls carry `x-peckboard-folder-id` so the plugin's
 * folder-scoped host functions run in THAT folder.
 *
 * As with `plugin-ui-panel.spec.ts`, the plugin-served page can't be compiled
 * in CI (no wasm32 toolchain), so the catalog and the page bytes are mocked and
 * this drives the host plumbing. `resolve_authed_scope`'s handling of the
 * header is covered by the `src/plugin/manager.rs` unit tests.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const PAGE_PATH = '/plugin-api/v1/demo-graph'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

test('a folder row opens its plugin page scoped to that folder', async ({
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
        sidebar_items: [],
        project_items: [],
        session_items: [],
        folder_items: [{ plugin: 'demo', id: 'demo-graph', label: 'Demo Graph', path: PAGE_PATH }],
      }),
    })
  })

  // Two folders, so the header the page sends has to be the row's own id and
  // not just "the first folder".
  await page.route('**/api/folders', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify([
        { id: 'folder-a', name: 'Alpha', path: '/tmp/alpha', created_at: '2024-01-01T00:00:00Z' },
        { id: 'folder-b', name: 'Beta', path: '/tmp/beta', created_at: '2024-01-01T00:00:00Z' },
      ]),
    })
  })

  // The bytes the plugin's own /plugin-api route would serve. The page asks the
  // parent to make one authed call; the reply echoes what it got back.
  await page.route(`**${PAGE_PATH}*`, async (route) => {
    await route.fulfill({
      contentType: 'text/html',
      body: `<!doctype html><html><body>
<h1 data-testid="folder-page-body">Demo graph page</h1>
<pre data-testid="folder-page-scope">pending</pre>
<script>
  window.addEventListener('message', (e) => {
    if (e.data && e.data.type === 'plugin-ui-fetch-result') {
      document.querySelector('[data-testid="folder-page-scope"]').textContent = e.data.body
    }
  })
  parent.postMessage(
    { type: 'plugin-ui-fetch', requestId: 1, method: 'GET', path: '/api/plugin-ui/demo/scope' },
    '*',
  )
</script>
</body></html>`,
    })
  })

  // Stand in for the plugin's authed route: report the folder header core would
  // have resolved the scope from.
  await page.route('**/api/plugin-ui/demo/scope', async (route) => {
    const folderId = route.request().headers()['x-peckboard-folder-id'] ?? 'none'
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({ folder_id: folderId }),
    })
  })

  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/folders')

  // Every declared folder item is offered in each folder row's menu.
  const rowFor = (name: string) =>
    page.locator('.list-view-row').filter({ has: page.getByTestId(`folder-row-${name}`) })
  await expect(rowFor('Alpha')).toBeVisible({ timeout: 10_000 })
  await rowFor('Alpha').locator('.list-view-menu').click()
  await expect(page.getByRole('menuitem', { name: 'Demo Graph' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(page.getByRole('menuitem', { name: 'Demo Graph' })).toHaveCount(0)

  await rowFor('Beta').locator('.list-view-menu').click()
  await page.screenshot({ path: 'e2e/test-results/folders-plugin-items.png' })
  await page.getByRole('menuitem', { name: 'Demo Graph' }).click()

  // Deep-linkable route, same shape as the project/session plugin pages.
  await expect(page).toHaveURL(/\/folders\/folder-b\/plugin\/demo-graph$/)

  const frameEl = page.getByTestId('plugin-fullpage-frame')
  await expect(frameEl).toHaveAttribute('src', new RegExp(`^${PAGE_PATH}`))

  const frame = page.frameLocator('[data-testid="plugin-fullpage-frame"]')
  await expect(frame.getByTestId('folder-page-body')).toContainText('Demo graph page')
  // The row's own folder id reached the backend — Beta's, not Alpha's.
  await expect(frame.getByTestId('folder-page-scope')).toContainText('"folder_id":"folder-b"')

  // Back returns to the folder list, with the folder settings still there.
  await page.getByRole('button', { name: '← Back' }).click()
  await expect(page).toHaveURL(/\/folders$/)
  await expect(page.getByRole('heading', { name: 'Folders', exact: true })).toBeVisible()
  await expect(rowFor('Alpha')).toBeVisible()
})
