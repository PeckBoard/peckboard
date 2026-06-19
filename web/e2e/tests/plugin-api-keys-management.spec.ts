import { test, expect, type APIRequestContext, type Route } from '@playwright/test'

/**
 * E2E for the API plugin's user-menu key-management flow.
 *
 * Per the user's fixture decision (PM 3bf9e0b3), plugin UI-panel e2e tests use
 * a **lighter, non-WASM stub** — a fake `ui_panel` registered by the test
 * harness plus a harness-served panel page — rather than compiling and loading
 * a real `.wasm` plugin. The repo's no-wasm32-in-CI convention stands. This
 * exercises the user-visible UI + host + iframe plumbing (the generic
 * `ui_panels` rendering in the user dropdown menu, the embedded sandboxed
 * iframe, and the create/list/use/revoke key flow the plugin's page drives) —
 * NOT the real WASM manifest/auth path, which is covered by the plugin crate's
 * own design + the Rust unit/integration tests.
 *
 * Mocking the catalog and the `/plugin-api/*` responses via `page.route` also
 * means the iframe renders the Playwright-fulfilled page, whose headers carry
 * no `X-Frame-Options`, so it is independent of core's (still-global)
 * `frame-ancestors 'none'` framing policy.
 *
 * Mirrors the host-plumbing coverage in `plugin-ui-panel.spec.ts` (which drives
 * the Settings → Plugins surface); this one drives the **user dropdown menu**
 * surface and the full key lifecycle.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const PANEL_PATH = '/plugin-api/v1/admin'
const SECRET = 'pba_0123456789abcdef0123456789abcdef0123456789abcdef'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { token: string }).token
}

/**
 * A minimal stand-in for the plugin's served management page. It links nothing
 * external (inline script is fine — this mocked response carries no CSP), and
 * drives the same `/plugin-api/v1/keys` + `/cards` calls the real page does, so
 * the host/iframe flow is exercised end to end against the stubbed endpoints.
 */
const PANEL_HTML = `<!doctype html><html><head><meta charset="utf-8"></head><body>
  <input id="adminKey" type="password" />
  <label><input type="checkbox" class="scope" value="read"> read</label>
  <button id="create">Create key</button>
  <div id="secretBox" hidden><code data-testid="secret"></code></div>
  <div data-testid="cards-status"></div>
  <div id="keys" data-testid="keys"></div>
  <script>
  (function () {
    var base = '/plugin-api/v1'
    function key() { return document.getElementById('adminKey').value || 'admin-key' }
    async function api(method, path, body) {
      var headers = { Authorization: 'Bearer ' + key() }
      if (body) headers['Content-Type'] = 'application/json'
      var res = await fetch(base + path, { method: method, headers: headers,
        body: body ? JSON.stringify(body) : undefined })
      var data = null; try { data = await res.json() } catch (e) {}
      return { status: res.status, data: data }
    }
    async function checkCards(secret) {
      var res = await fetch(base + '/cards', { headers: { Authorization: 'Bearer ' + secret } })
      document.querySelector('[data-testid="cards-status"]').textContent = String(res.status)
    }
    async function refresh() {
      var r = await api('GET', '/keys')
      var keys = (r.data && r.data.keys) || []
      var wrap = document.getElementById('keys')
      wrap.innerHTML = ''
      keys.forEach(function (k) {
        var row = document.createElement('div')
        row.setAttribute('data-testid', 'key-row')
        row.textContent = (k.label || '—') + ' ' + (k.masked || '')
        var btn = document.createElement('button')
        btn.setAttribute('data-testid', 'revoke')
        btn.textContent = 'Revoke'
        btn.addEventListener('click', async function () {
          await api('DELETE', '/keys/' + encodeURIComponent(k.id))
          await refresh()
          await checkCards(window.__lastSecret || 'gone')
        })
        row.appendChild(btn)
        wrap.appendChild(row)
      })
    }
    document.getElementById('create').addEventListener('click', async function () {
      var scopes = Array.prototype.map.call(document.querySelectorAll('.scope:checked'),
        function (c) { return c.value })
      var r = await api('POST', '/keys', { label: 'e2e read key', scopes: scopes })
      if (r.status === 201 && r.data && r.data.key) {
        window.__lastSecret = r.data.key
        var box = document.getElementById('secretBox')
        box.hidden = false
        box.querySelector('[data-testid="secret"]').textContent = r.data.key
        await refresh()
        await checkCards(r.data.key)
      }
    })
    refresh()
  })()
  </script>
</body></html>`

test('API Keys panel: user-menu link → create, use, and revoke a key', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)

  // ── Stateful in-test key store backing the mocked plugin endpoints ──
  type Key = { id: string; label: string; scopes: string[]; masked: string; secret: string }
  const keys: Key[] = []

  // Catalog: declare one plugin UI panel so the host renders its user-menu link.
  await page.route('**/api/plugins', (route) =>
    route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        plugins: [],
        ui_panels: [{ plugin: 'api', id: 'api-keys', title: 'API Keys', path: PANEL_PATH }],
      }),
    }),
  )

  // The plugin-served management page (what the iframe loads).
  await page.route(`**${PANEL_PATH}`, (route) =>
    route.fulfill({ contentType: 'text/html', body: PANEL_HTML }),
  )

  // CORS-friendly JSON fulfil — the iframe runs at an opaque origin, so its
  // `/plugin-api` fetches are cross-origin and need ACAO + preflight handling
  // (exactly what the real plugin emits).
  const cors = { 'access-control-allow-origin': '*' }
  const json = (route: Route, status: number, body: unknown) =>
    route.fulfill({
      status,
      headers: { ...cors, 'content-type': 'application/json' },
      body: JSON.stringify(body),
    })
  const preflight = (route: Route) =>
    route.fulfill({
      status: 204,
      headers: {
        ...cors,
        'access-control-allow-methods': 'GET,POST,DELETE,OPTIONS',
        'access-control-allow-headers': 'authorization,content-type',
      },
    })

  // Key management endpoints (admin): list / create / revoke.
  await page.route('**/plugin-api/v1/keys', async (route) => {
    const req = route.request()
    if (req.method() === 'OPTIONS') return preflight(route)
    if (req.method() === 'GET') {
      return json(route, 200, {
        keys: keys.map((k) => ({ id: k.id, label: k.label, scopes: k.scopes, masked: k.masked })),
      })
    }
    if (req.method() === 'POST') {
      const k: Key = {
        id: `id-${keys.length + 1}`,
        label: 'e2e read key',
        scopes: ((req.postDataJSON() as { scopes?: string[] }).scopes ?? []) as string[],
        masked: `${SECRET.slice(0, 8)}…`,
        secret: SECRET,
      }
      keys.push(k)
      // Create returns the full secret exactly once.
      return json(route, 201, { ...k, key: k.secret })
    }
    return json(route, 405, { error: 'method not allowed' })
  })
  await page.route('**/plugin-api/v1/keys/*', async (route) => {
    const req = route.request()
    if (req.method() === 'OPTIONS') return preflight(route)
    const id = decodeURIComponent(new URL(req.url()).pathname.split('/').pop() ?? '')
    const idx = keys.findIndex((k) => k.id === id)
    if (idx === -1) return json(route, 404, { error: 'unknown key' })
    keys.splice(idx, 1)
    return json(route, 200, { deleted: id })
  })

  // Data route: 200 with a valid live key, 401 otherwise (so revoke → 401).
  await page.route('**/plugin-api/v1/cards', async (route) => {
    const req = route.request()
    if (req.method() === 'OPTIONS') return preflight(route)
    const auth = (await req.allHeaders())['authorization'] ?? ''
    const presented = auth.replace(/^Bearer\s+/i, '')
    const live = keys.some((k) => k.secret === presented)
    return live ? json(route, 200, { cards: [] }) : json(route, 401, { error: 'invalid key' })
  })

  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  // App fetches the panel catalog once authenticated; wait for it so the
  // user-menu link is rendered deterministically (no cold-start race).
  const catalog = page.waitForResponse((r) => r.url().includes('/api/plugins'))
  await page.goto('/')
  await catalog

  // The plugin's "API Keys" link is present in the user dropdown menu.
  await page.getByRole('button', { name: 'User menu' }).click()
  const link = page.getByTestId('user-menu-plugin-api-api-keys')
  await expect(link).toBeVisible()

  // Opening it embeds the plugin-served page in the sandboxed iframe.
  await link.click()
  await expect(page.getByTestId('plugin-panel-modal')).toBeVisible()
  const frameEl = page.getByTestId('plugin-panel-frame')
  await expect(frameEl).toHaveAttribute('src', PANEL_PATH)
  await expect(frameEl).toHaveAttribute('sandbox', 'allow-scripts allow-forms allow-popups')
  const panel = page.frameLocator('[data-testid="plugin-panel-frame"]')

  // Create a read-only key; its secret is shown exactly once.
  await panel.locator('.scope[value="read"]').check()
  await panel.locator('#create').click()
  await expect(panel.getByTestId('secret')).toHaveText(SECRET)

  // It appears in the list, and works against the read data route (200).
  await expect(panel.getByTestId('key-row')).toHaveCount(1)
  await expect(panel.getByTestId('cards-status')).toHaveText('200')

  // Revoke it (confirm dialog auto-accepted): it disappears and now 401s.
  page.on('dialog', (d) => d.accept())
  await panel.getByTestId('revoke').click()
  await expect(panel.getByTestId('key-row')).toHaveCount(0)
  await expect(panel.getByTestId('cards-status')).toHaveText('401')
})
