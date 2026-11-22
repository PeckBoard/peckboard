import { test, expect, type APIRequestContext, type Page } from '../harness'

/**
 * The "Experimental" stability badge: a plugin whose manifest or registry
 * entry declares `stability: "experimental"` is badged in the installed
 * plugins list, the registry Browse list + detail modal, and its own page
 * header; a stable plugin is not. The catalog, registry, and plugin page are
 * mocked (same approach as plugin-registry.spec.ts / plugin-page-handoff.spec.ts)
 * — field parsing is covered by Rust tests.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const REPO_URL = 'https://raw.githubusercontent.com/PeckBoard/plugins/main/registry.json'
const PAGE_PATH = '/plugin-api/v1/homekit'
const TITLE = 'This plugin is experimental and may change or break'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

function wasmPlugin(name: string, stability?: 'experimental') {
  return {
    name,
    description: `The ${name} plugin.`,
    version: '1.0.0',
    repository: `https://github.com/PeckBoard/${name}`,
    hooks: ['http.request.authed'],
    permissions: [],
    status: 'approved',
    ...(stability ? { stability } : {}),
  }
}

function registryPlugin(id: string, stability?: 'experimental') {
  return {
    id,
    name: id === 'homekit' ? 'HomeKit' : 'Demo Plugin',
    description: `The ${id} plugin.`,
    author: 'PeckBoard',
    version: '1.0.0',
    hooks: ['http.request.authed'],
    repository: REPO_URL,
    repository_label: 'PeckBoard/plugins',
    installed: false,
    compatible: true,
    ...(stability ? { stability } : {}),
  }
}

async function mockCatalog(page: Page) {
  await page.route('**/api/plugins', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        plugins: [],
        ui_panels: [],
        wasm_plugins: [wasmPlugin('homekit', 'experimental'), wasmPlugin('demo')],
        sidebar_items: [{ plugin: 'homekit', id: 'homekit', label: 'HomeKit', path: PAGE_PATH }],
      }),
    })
  })
  await page.route('**/api/plugins/registry', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        repositories: [{ url: REPO_URL, label: 'PeckBoard/plugins', removable: true, ok: true }],
        plugins: [registryPlugin('homekit', 'experimental'), registryPlugin('demo')],
        mcp_servers: [],
      }),
    })
  })
  await page.route(`**${PAGE_PATH}*`, async (route) => {
    await route.fulfill({
      contentType: 'text/html',
      body: '<!doctype html><html><body><p>HomeKit page</p></body></html>',
    })
  })
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

test('experimental plugins are badged in the lists and on their page', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await mockCatalog(page)

  // Installed plugins list.
  await loadAppAt(page, token, '/plugins')
  await expect(page.getByTestId('plugins-section')).toBeVisible({ timeout: 10_000 })
  const installedBadge = page.getByTestId('wasm-plugin-experimental-homekit')
  await expect(installedBadge).toBeVisible()
  await expect(installedBadge).toHaveText(/experimental/i)
  await expect(installedBadge).toHaveAttribute('title', TITLE)
  await expect(page.getByTestId('wasm-plugin-experimental-demo')).toHaveCount(0)

  // Registry Browse list + detail modal.
  await page.getByTestId('browse-plugins').click()
  await expect(page.getByTestId('plugin-registry-panel')).toBeVisible()
  const registryBadge = page.getByTestId('registry-experimental-badge-homekit')
  await expect(registryBadge).toBeVisible()
  await expect(registryBadge).toHaveAttribute('title', TITLE)
  await expect(page.getByTestId('registry-experimental-badge-demo')).toHaveCount(0)
  await page.getByTestId('registry-open-homekit').click()
  await expect(
    page.getByTestId('registry-detail-modal').getByTestId('registry-detail-experimental'),
  ).toBeVisible()

  // The plugin's own page header.
  await page.goto('/plugin-page/homekit/homekit')
  await expect(page.getByTestId('plugin-fullpage')).toBeVisible({ timeout: 10_000 })
  const headerBadge = page.getByTestId('plugin-fullpage-experimental')
  await expect(headerBadge).toBeVisible()
  await expect(headerBadge).toHaveAttribute('title', TITLE)
})
