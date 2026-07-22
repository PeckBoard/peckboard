import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

/**
 * Docs screenshot capture — NOT a test of app behaviour.
 *
 * Boots peckboard through the regular e2e harness (mock:* models only,
 * fresh temp data dir, bootstrap admin), seeds data so the screens look
 * real, and writes PNGs with stable names into
 * `docs/assets/screenshots/` for the public docs site to reference:
 *
 *   - board.png             — per-project kanban with cards across columns
 *   - chat.png              — a chat session with a completed mock agent run
 *   - experts.png           — the experts view with knowledge/question/PM experts
 *   - project.png           — the projects overview list
 *   - plugin-registry.png   — Settings → Plugin Registry browse (real registry.json)
 *   - playwright-player.png — the Playwright Tests replay player mid-run
 *   - providers.png         — Settings → Providers & Accounts with accounts
 *
 * The last three run against the same live server but stub the relevant
 * API routes in the page (same convention as the tests/ specs): the
 * registry from the in-repo `plugins/registry.json`, the replay player
 * with a seeded run whose frames are captured live from a fake shop
 * page, and the account lists with realistic entries.
 *
 * Re-run with `cd web && npm run screenshots` (after `npm install` and
 * the one-time `npm run e2e:install`). The run is idempotent: it boots
 * on its own ports (4446/4447, so a leftover e2e server on 4444 is
 * never reused), seeds from scratch, and overwrites the PNGs.
 * Viewport is fixed at 1280x800 by playwright.screenshots.config.ts.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const OUT_DIR = path.resolve(HERE, '..', '..', '..', 'docs', 'assets', 'screenshots')

type AuthBundle = { token: string; authHeader: { Authorization: string } }

async function authenticate(request: APIRequestContext): Promise<AuthBundle> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

/** Temp source tree with sizeable topic dirs so `spin_up_experts`
 *  partitions it into a couple of knowledge experts. */
function makeSourceTree(): string {
  const root = mkdtempSync(path.join(tmpdir(), 'peckboard-shots-src-'))
  const body = 'pub fn handler() { /* logic */ }\n'.repeat(1400)
  for (const dir of ['api', 'frontend']) {
    mkdirSync(path.join(root, dir), { recursive: true })
    writeFileSync(path.join(root, dir, 'mod.rs'), body)
  }
  return root
}

/** Read the MCP bearer token the server wrote for a session. */
function readMcpToken(sessionId: string): string {
  const dataDir = process.env.PECKBOARD_E2E_DATA_DIR
  expect(dataDir, 'PECKBOARD_E2E_DATA_DIR exported by playwright.config.ts').toBeTruthy()
  const cfgPath = path.join(dataDir!, 'worker-mcp', `${sessionId}.json`)
  const cfg = JSON.parse(readFileSync(cfgPath, 'utf8')) as {
    mcpServers: { peckboard: { headers: { Authorization: string } } }
  }
  return cfg.mcpServers.peckboard.headers.Authorization.replace(/^Bearer /, '')
}

async function createFolder(
  request: APIRequestContext,
  authHeader: AuthBundle['authHeader'],
  name: string,
  dirPath: string,
): Promise<string> {
  const res = await request.post('/api/folders', {
    headers: authHeader,
    data: { name, path: dirPath },
  })
  expect(res.ok(), `create folder ${name} failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function createProject(
  request: APIRequestContext,
  authHeader: AuthBundle['authHeader'],
  name: string,
  folderId: string,
): Promise<string> {
  const res = await request.post('/api/projects', {
    headers: authHeader,
    // worker_count: 0 so the orchestrator never picks cards up and
    // mutates their step mid-capture; mock:echo so any expert capture
    // dispatch stays on the mock provider.
    data: { name, folder_id: folderId, model: 'mock:echo', workflow: 'task', worker_count: 0 },
  })
  expect(res.ok(), `create project ${name} failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function createCard(
  request: APIRequestContext,
  authHeader: AuthBundle['authHeader'],
  projectId: string,
  title: string,
  description: string,
  priority: number,
  step: string,
): Promise<void> {
  const res = await request.post(`/api/projects/${projectId}/cards`, {
    headers: authHeader,
    data: { title, description, step: 'backlog', priority },
  })
  expect(res.ok(), `create card ${title} failed: ${await res.text()}`).toBeTruthy()
  if (step !== 'backlog') {
    const card = (await res.json()) as { id: string }
    const move = await request.put(`/api/projects/${projectId}/cards/${card.id}`, {
      headers: authHeader,
      data: { step },
    })
    expect(move.ok(), `move card ${title} to ${step} failed: ${await move.text()}`).toBeTruthy()
  }
}

/** Poll the session event log until `count` agent runs have completed. */
async function waitForAgentEnds(
  request: APIRequestContext,
  authHeader: AuthBundle['authHeader'],
  sessionId: string,
  count: number,
): Promise<void> {
  await expect
    .poll(
      async () => {
        const res = await request.get(`/api/sessions/${sessionId}/events?limit=200`, {
          headers: authHeader,
        })
        if (!res.ok()) return 0
        const events = (await res.json()) as { kind: string }[]
        return events.filter((e) => e.kind === 'agent-end').length
      },
      { timeout: 30_000, message: `session ${sessionId}: waiting for ${count} agent-end(s)` },
    )
    .toBeGreaterThanOrEqual(count)
}

async function capture(page: Page, name: string): Promise<void> {
  await page.screenshot({
    path: path.join(OUT_DIR, name),
    animations: 'disabled',
    caret: 'hide',
  })
}

test('capture docs screenshots @screenshot', async ({ request, page, baseURL }) => {
  test.setTimeout(180_000)
  expect(baseURL, 'baseURL configured').toBeTruthy()
  mkdirSync(OUT_DIR, { recursive: true })

  const { token, authHeader } = await authenticate(request)

  // ── Seed: main project with cards across the kanban columns ──
  const srcDir = makeSourceTree()
  const folderA = await createFolder(request, authHeader, 'payments-service', srcDir)
  const projectA = await createProject(request, authHeader, 'Payments Service', folderA)

  const cards: [title: string, description: string, priority: number, step: string][] = [
    ['Add CSV export for invoices', 'Finance wants monthly exports.', 0, 'backlog'],
    ['Rate-limit public API endpoints', 'Protect /api from abusive clients.', 1, 'backlog'],
    ['Audit-log retention policy', 'Decide and enforce a retention window.', 2, 'backlog'],
    [
      'Fix flaky WebSocket reconnect',
      'Clients drop every few minutes on staging.',
      0,
      'in_progress',
    ],
    ['OAuth login with GitHub', 'Add GitHub as an identity provider.', 1, 'in_progress'],
    ['Refactor session storage layer', 'Split read/write paths before sharding.', 0, 'review'],
    ['Set up CI pipeline', 'Build, lint, and test on every push.', 0, 'done'],
    ['Bootstrap project skeleton', 'Initial repo layout and tooling.', 1, 'done'],
  ]
  for (const [title, description, priority, step] of cards) {
    await createCard(request, authHeader, projectA, title, description, priority, step)
  }

  // ── Seed: a second project so the overview list has some depth ──
  const folderB = await createFolder(
    request,
    authHeader,
    'mobile-app',
    mkdtempSync(path.join(tmpdir(), 'peckboard-shots-mobile-')),
  )
  await createProject(request, authHeader, 'Mobile App Revamp', folderB)

  // ── Seed: a chat session with two completed mock agent runs ──
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'Fix flaky WebSocket reconnect', folder_id: folderA },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  const msg1 = await request.post(`/api/sessions/${session.id}/message`, {
    headers: authHeader,
    data: {
      text: 'The WebSocket client drops and reconnects every few minutes on staging — can you investigate?',
      model: 'mock:happy-path',
    },
  })
  expect(msg1.ok(), `first message failed: ${await msg1.text()}`).toBeTruthy()
  await waitForAgentEnds(request, authHeader, session.id, 1)

  const msg2 = await request.post(`/api/sessions/${session.id}/message`, {
    headers: authHeader,
    data: {
      text: 'Great — now add a regression test that covers the reconnect path.',
      model: 'mock:happy-path',
    },
  })
  expect(msg2.ok(), `second message failed: ${await msg2.text()}`).toBeTruthy()
  await waitForAgentEnds(request, authHeader, session.id, 2)

  // ── Seed: knowledge experts on the main project via spin_up_experts ──
  // The tool ships in the experts WASM plugin (staged into the data dir
  // by playwright.screenshots.config.ts when peck-plugins/experts/dist
  // exists); it loads inert, so approve it first. Without the wasm the
  // experts seeding — and experts.png — is skipped, keeping the rest of
  // the captures alive on machines that haven't built the plugin.
  const approval = await request.post('/api/plugins/experts/approval', {
    headers: authHeader,
    data: { decision: 'approve' },
  })
  const expertsAvailable = approval.ok()
  if (!expertsAvailable) {
    console.warn(
      `[screenshots] experts plugin not loaded (approval HTTP ${approval.status()}) — ` +
        'skipping experts seeding and the experts.png capture. Build ' +
        'peck-plugins/experts (npm run build) to restore it.',
    )
  }
  if (expertsAvailable) {
    const mcpToken = readMcpToken(session.id)
    const mcpRes = await request.post('/mcp', {
      headers: { Authorization: `Bearer ${mcpToken}` },
      data: {
        jsonrpc: '2.0',
        id: 1,
        method: 'tools/call',
        params: {
          name: 'spin_up_experts',
          arguments: { project_id: projectA, max_experts: 3 },
        },
      },
    })
    expect(mcpRes.ok(), `spin_up_experts failed: ${await mcpRes.text()}`).toBeTruthy()
    const mcpJson = (await mcpRes.json()) as { error?: { message: string } }
    expect(mcpJson.error, `spin_up_experts error: ${JSON.stringify(mcpJson.error)}`).toBeFalsy()
  }

  // ── Capture ──
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t as string)
  }, token)

  // board.png — per-project kanban with cards across columns.
  await page.goto(`/projects/${projectA}`)
  await expect(page.locator('.rail-status.online')).toBeVisible({ timeout: 15_000 })
  const columnByLabel = (label: string) =>
    page.locator('.kanban-column').filter({
      has: page.locator('.kanban-column-header h3', { hasText: new RegExp(`^${label}$`) }),
    })
  await expect(
    columnByLabel('In Progress').locator('.kanban-card-title', {
      hasText: 'Fix flaky WebSocket reconnect',
    }),
  ).toBeVisible({ timeout: 10_000 })
  await expect(
    columnByLabel('Done').locator('.kanban-card-title', { hasText: 'Set up CI pipeline' }),
  ).toBeVisible()
  await capture(page, 'board.png')

  // project.png — the projects overview list.
  await page.goto('/projects')
  await expect(page.locator('.list-view-name', { hasText: 'Payments Service' })).toBeVisible({
    timeout: 10_000,
  })
  await expect(page.locator('.list-view-name', { hasText: 'Mobile App Revamp' })).toBeVisible()
  await capture(page, 'project.png')

  // chat.png — the seeded session's transcript.
  await page.goto(`/sessions/${session.id}`)
  await expect(page.getByText('Done.').last()).toBeVisible({ timeout: 10_000 })
  await capture(page, 'chat.png')

  // experts.png — the experts view, now a plugin-served page in an
  // iframe (the experts plugin's `experts` sidebar item).
  if (expertsAvailable) {
    await page.goto('/plugin-page/experts/experts')
    const expertsView = page.frameLocator('[data-testid="plugin-fullpage-frame"]')
    await expect(expertsView.locator('.expert-name').first()).toBeVisible({ timeout: 15_000 })
    await capture(page, 'experts.png')
  }
})

// ─────────────────────────────────────────────────────────────────────
// Additional captures. Each stubs its API routes in the page (the same
// convention the tests/ specs use) so the screens are rich, stable, and
// free of live network / provider dependencies.
// ─────────────────────────────────────────────────────────────────────

const REPO_URL = 'https://raw.githubusercontent.com/PeckBoard/plugins/main/registry.json'
const REGISTRY_JSON = path.resolve(HERE, '..', '..', '..', '..', 'plugins', 'registry.json')

/** The replay page html served by the playwright-video plugin. Its
 *  `PAGE` export is one giant template literal that by design contains
 *  no backticks, `${`, or backslash escapes (see the file's header), so
 *  the raw literal body IS the html — extract it from the source text
 *  rather than importing across the CJS package boundary (which the
 *  test-runner's TS loader refuses to compile). */
const PLAYWRIGHT_TESTS_PAGE = (() => {
  const src = readFileSync(
    path.resolve(
      HERE,
      '..',
      '..',
      '..',
      '..',
      'peck-plugins',
      'playwright-video',
      'src',
      'page.ts',
    ),
    'utf8',
  )
  const start = src.indexOf('`') + 1
  const end = src.lastIndexOf('`')
  if (start <= 0 || end <= start) throw new Error('PAGE literal not found in page.ts')
  return src.slice(start, end)
})()

async function loadAppAt(page: Page, token: string, route: string): Promise<void> {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t as string)
  }, token)
  await page.goto(route)
}

/** An empty installed-plugins catalog (the registry page only needs it to
 *  resolve `installed` states, which the registry payload already carries). */
async function stubEmptyCatalog(page: Page): Promise<void> {
  await page.route('**/api/plugins', (route) =>
    route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({ plugins: [], ui_panels: [], wasm_plugins: [] }),
    }),
  )
}

// plugin-registry.png — Settings → Plugin Registry, serving the real
// in-repo registry.json (mapped to the aggregate-browse wire shape) so
// the shot always matches the actual distributed catalog.
test('capture plugin registry screenshot @screenshot', async ({ request, page }) => {
  mkdirSync(OUT_DIR, { recursive: true })
  const { token } = await authenticate(request)

  const reg = JSON.parse(readFileSync(REGISTRY_JSON, 'utf8')) as {
    plugins: Record<string, unknown>[]
    mcp_servers: Record<string, unknown>[]
  }
  const fromRepo = { repository: REPO_URL, repository_label: 'PeckBoard/plugins' }
  const registryPayload = {
    repositories: [{ url: REPO_URL, label: 'PeckBoard/plugins', removable: false, ok: true }],
    plugins: reg.plugins.map((p) => ({
      ...p,
      ...fromRepo,
      installed: p.id === 'experts' || p.id === 'playwright-video',
      compatible: true,
    })),
    mcp_servers: reg.mcp_servers.map((m) => ({
      command: '',
      args: [],
      env: [],
      url: '',
      headers: [],
      setup_note: '',
      ...m,
      ...fromRepo,
      compatible: true,
    })),
  }

  await stubEmptyCatalog(page)
  await page.route('**/api/plugins/registry', (route) =>
    route.fulfill({ contentType: 'application/json', body: JSON.stringify(registryPayload) }),
  )

  await loadAppAt(page, token, '/plugin-registry')
  await expect(page.getByTestId('plugin-registry-panel')).toBeVisible({ timeout: 10_000 })
  await expect(page.getByTestId('registry-plugin-experts')).toBeVisible()
  await expect(page.getByTestId('registry-mcp-playwright')).toBeVisible()
  await capture(page, 'plugin-registry.png')
})

// ── playwright-player.png ────────────────────────────────────────────

/** The fake "app under test" whose screenshots become the replay frames.
 *  Three states, driven by a body class: initial grid, item added
 *  (badge + toast), checkout drawer open. */
const FAKE_APP = `<!doctype html>
<html><head><meta charset="utf-8"><style>
  * { margin: 0; box-sizing: border-box; font-family: system-ui, sans-serif }
  body { background: #f6f7f9; color: #1c2733 }
  header { display: flex; align-items: center; gap: 18px; padding: 13px 28px; background: #fff; border-bottom: 1px solid #e3e7ec }
  .logo { font-weight: 700; font-size: 17px; color: #0b6e4f }
  nav { display: flex; gap: 16px; font-size: 13px; color: #5b6773 }
  .cart { margin-left: auto; position: relative; font-size: 13px; padding: 7px 14px; border: 1px solid #d6dce3; border-radius: 8px; background: #fff }
  .badge { display: none; position: absolute; top: -7px; right: -7px; background: #0b6e4f; color: #fff; border-radius: 50%; width: 18px; height: 18px; font-size: 11px; line-height: 18px; text-align: center }
  body.added .badge, body.checkout .badge { display: block }
  main { max-width: 900px; margin: 24px auto; padding: 0 20px }
  h1 { font-size: 21px; margin-bottom: 4px }
  .sub { color: #5b6773; font-size: 13px; margin-bottom: 16px }
  .product-grid { display: grid; grid-template-columns: repeat(3, 1fr); gap: 16px }
  .product-card { background: #fff; border: 1px solid #e3e7ec; border-radius: 10px; padding: 14px }
  .thumb { height: 104px; border-radius: 8px; margin-bottom: 11px }
  .t1 { background: linear-gradient(135deg, #ffd97d, #ff9f68) }
  .t2 { background: linear-gradient(135deg, #a8e6cf, #56c596) }
  .t3 { background: linear-gradient(135deg, #c3d9ff, #7f9cf5) }
  .name { font-weight: 600; font-size: 14px }
  .price { color: #5b6773; font-size: 12px; margin: 4px 0 11px }
  .add-btn { width: 100%; padding: 8px 0; border: 0; border-radius: 8px; background: #0b6e4f; color: #fff; font-size: 12px }
  .toast { display: none; position: fixed; bottom: 20px; left: 50%; transform: translateX(-50%); background: #1c2733; color: #fff; font-size: 13px; padding: 10px 18px; border-radius: 8px }
  body.added .toast { display: block }
  .drawer { display: none; position: fixed; top: 0; right: 0; bottom: 0; width: 340px; background: #fff; border-left: 1px solid #e3e7ec; padding: 22px; flex-direction: column; gap: 12px; box-shadow: -12px 0 32px rgba(16, 24, 32, .08) }
  body.checkout .drawer { display: flex }
  .drawer h2 { font-size: 16px }
  .order-summary { font-size: 13px; color: #37424e; display: flex; flex-direction: column; gap: 7px }
  .order-summary div { display: flex; justify-content: space-between }
  .coupon-row { display: flex; gap: 8px }
  .coupon-row input { flex: 1; padding: 8px 10px; border: 1px solid #d6dce3; border-radius: 8px; font-size: 13px }
  .coupon-row button { padding: 8px 12px; border: 1px solid #d6dce3; border-radius: 8px; background: #fff; font-size: 12px }
  .coupon-err { color: #c0392b; font-size: 12px }
  .place { margin-top: auto; padding: 11px 0; border: 0; border-radius: 8px; background: #0b6e4f; color: #fff; font-size: 14px }
</style></head><body>
  <header>
    <span class="logo">Birdseed &amp; Co.</span>
    <nav><span>Shop</span><span>Feeders</span><span>About</span></nav>
    <button class="cart" id="checkout-btn">Cart<span class="badge">1</span></button>
  </header>
  <main>
    <h1>Premium seed mixes</h1>
    <div class="sub">Small-batch blends, milled weekly.</div>
    <div class="product-grid">
      <div class="product-card"><div class="thumb t1"></div><div class="name">Golden Millet Blend</div><div class="price">$8.50 / lb</div><button class="add-btn">Add to cart</button></div>
      <div class="product-card"><div class="thumb t2"></div><div class="name">Sunflower Mix</div><div class="price">$11.00 / lb</div><button class="add-btn">Add to cart</button></div>
      <div class="product-card"><div class="thumb t3"></div><div class="name">Winter Suet Pellets</div><div class="price">$9.25 / lb</div><button class="add-btn">Add to cart</button></div>
    </div>
  </main>
  <div class="toast">Added to cart — Sunflower Mix</div>
  <aside class="drawer">
    <h2>Your order</h2>
    <div class="order-summary">
      <div><span>Sunflower Mix × 1</span><span>$11.00</span></div>
      <div><span>Shipping</span><span>$4.90</span></div>
      <div><strong>Total</strong><strong>$15.90</strong></div>
    </div>
    <div class="coupon-row"><input id="coupon" value="BIRD10"><button id="apply-coupon">Apply</button></div>
    <div class="coupon-err">Coupon service unavailable — try again later.</div>
    <button class="place">Place order</button>
  </aside>
</body></html>`

// playwright-player.png — the Playwright Tests plugin page (run list +
// replay player), served from the real plugin's PAGE html with a seeded
// run. The replay frames are genuine screenshots of FAKE_APP, captured
// here in a throwaway page, so the stage shows a believable app.
test('capture playwright player screenshot @screenshot', async ({ request, page }) => {
  mkdirSync(OUT_DIR, { recursive: true })
  const { token } = await authenticate(request)

  // Catalog: the plugin is installed + approved and contributes its
  // left-rail entry (which /plugin-page/... resolves against).
  await page.route('**/api/plugins', (route) =>
    route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        plugins: [],
        ui_panels: [],
        wasm_plugins: [
          {
            name: 'playwright-video',
            description: 'LogRocket-style replay of recorded browser test runs.',
            version: '0.3.2',
            repository: 'https://github.com/PeckBoard/playwright-video',
            hooks: ['http.request.before', 'http.request.authed'],
            permissions: ['contribute_sidebar', 'browser_runs_read', 'user_authority'],
            status: 'approved',
            error: null,
          },
        ],
        sidebar_items: [
          {
            plugin: 'playwright-video',
            id: 'playwright-tests',
            label: 'Playwright Tests',
            path: '/plugin-api/v1/playwright-video',
          },
        ],
        project_items: [],
        session_items: [],
      }),
    }),
  )

  // The iframe src — the real plugin page html.
  await page.route('**/plugin-api/v1/playwright-video', (route) =>
    route.fulfill({ contentType: 'text/html; charset=utf-8', body: PLAYWRIGHT_TESTS_PAGE }),
  )

  // Capture the three replay frames from the fake app.
  const app = await page.context().newPage()
  await app.setViewportSize({ width: 1024, height: 640 })
  await app.setContent(FAKE_APP)
  const frameShots: Record<string, string> = {}
  frameShots['f1.png'] = (await app.screenshot()).toString('base64')
  await app.evaluate(() => document.body.classList.add('added'))
  frameShots['f2.png'] = (await app.screenshot()).toString('base64')
  await app.evaluate(() => {
    document.body.classList.remove('added')
    document.body.classList.add('checkout')
  })
  frameShots['f3.png'] = (await app.screenshot()).toString('base64')
  await app.close()

  // One finished run, ~7 minutes old: add to cart → checkout → a coupon
  // that rage-clicks into a 500 → order placed anyway.
  const t0 = Date.now() - 7 * 60_000
  const BASE = 'http://localhost:5173'
  const run = {
    id: 'run-checkout',
    name: 'checkout happy path — chromium',
    url: `${BASE}/shop`,
    session_id: 'ses-demo-1',
    project_id: null,
    card_id: null,
    started_ms: t0,
    ended_ms: t0 + 14_100,
    steps: [
      { n: 1, ts_ms: t0, action: 'open', detail: { url: `${BASE}/shop` }, frame: 'f1.png' },
      { n: 2, ts_ms: t0 + 900, action: 'wait_selector', detail: { text: '.product-grid' } },
      {
        n: 3,
        ts_ms: t0 + 2_600,
        action: 'click',
        target: '.product-card:nth-child(2) .add-btn',
        frame: 'f2.png',
      },
      { n: 4, ts_ms: t0 + 4_400, action: 'click', target: '#checkout-btn', frame: 'f3.png' },
      { n: 5, ts_ms: t0 + 6_200, action: 'fill', target: '#coupon', detail: { text: 'BIRD10' } },
      { n: 6, ts_ms: t0 + 7_100, action: 'click', target: '#apply-coupon' },
      { n: 7, ts_ms: t0 + 7_500, action: 'click', target: '#apply-coupon' },
      { n: 8, ts_ms: t0 + 7_900, action: 'click', target: '#apply-coupon' },
      { n: 9, ts_ms: t0 + 11_800, action: 'wait_selector', detail: { text: '.order-summary' } },
      { n: 10, ts_ms: t0 + 12_900, action: 'screenshot', frame: 'f3.png' },
    ],
    network: [
      {
        id: 1,
        ts_ms: t0 + 70,
        dur_ms: 190,
        method: 'GET',
        url: `${BASE}/shop`,
        resource_type: 'document',
        status: 200,
        size: 14_200,
      },
      {
        id: 2,
        ts_ms: t0 + 290,
        dur_ms: 110,
        method: 'GET',
        url: `${BASE}/assets/app.css`,
        resource_type: 'stylesheet',
        status: 200,
        size: 8_100,
      },
      {
        id: 3,
        ts_ms: t0 + 310,
        dur_ms: 240,
        method: 'GET',
        url: `${BASE}/assets/app.js`,
        resource_type: 'script',
        status: 200,
        size: 96_500,
      },
      {
        id: 4,
        ts_ms: t0 + 620,
        dur_ms: 340,
        method: 'GET',
        url: `${BASE}/api/products`,
        resource_type: 'xhr',
        status: 200,
        size: 5_230,
      },
      {
        id: 5,
        ts_ms: t0 + 2_650,
        dur_ms: 170,
        method: 'POST',
        url: `${BASE}/api/cart`,
        resource_type: 'xhr',
        status: 201,
        size: 412,
      },
      {
        id: 6,
        ts_ms: t0 + 4_450,
        dur_ms: 260,
        method: 'GET',
        url: `${BASE}/api/cart`,
        resource_type: 'xhr',
        status: 200,
        size: 980,
      },
      {
        id: 7,
        ts_ms: t0 + 7_150,
        dur_ms: 430,
        method: 'POST',
        url: `${BASE}/api/coupon`,
        resource_type: 'xhr',
        status: 500,
        size: 88,
        resp_body: '{"error":"coupon service unavailable"}',
      },
      {
        id: 8,
        ts_ms: t0 + 7_950,
        dur_ms: 380,
        method: 'POST',
        url: `${BASE}/api/coupon`,
        resource_type: 'xhr',
        status: 500,
        size: 88,
        resp_body: '{"error":"coupon service unavailable"}',
      },
      {
        id: 9,
        ts_ms: t0 + 12_000,
        dur_ms: 520,
        method: 'POST',
        url: `${BASE}/api/checkout`,
        resource_type: 'xhr',
        status: 200,
        size: 1_220,
      },
    ],
    console_events: [
      { ts_ms: t0 + 680, level: 'log', text: '12 products loaded' },
      {
        ts_ms: t0 + 7_600,
        level: 'error',
        text: 'POST /api/coupon failed: 500 coupon service unavailable',
      },
      { ts_ms: t0 + 12_550, level: 'log', text: 'order draft saved (#A-1042)' },
    ],
    pointer_events: [
      { ts_ms: t0 + 1_400, t: 'move', x: 512, y: 300, vw: 1024, vh: 640 },
      { ts_ms: t0 + 2_100, t: 'move', x: 628, y: 402, vw: 1024, vh: 640 },
      { ts_ms: t0 + 2_500, t: 'move', x: 652, y: 428, vw: 1024, vh: 640 },
      { ts_ms: t0 + 2_600, t: 'down', x: 652, y: 428, vw: 1024, vh: 640 },
      { ts_ms: t0 + 3_300, t: 'move', x: 730, y: 260, vw: 1024, vh: 640 },
      { ts_ms: t0 + 4_200, t: 'move', x: 905, y: 42, vw: 1024, vh: 640 },
      { ts_ms: t0 + 4_400, t: 'down', x: 905, y: 42, vw: 1024, vh: 640 },
      { ts_ms: t0 + 5_300, t: 'move', x: 762, y: 250, vw: 1024, vh: 640 },
      { ts_ms: t0 + 6_100, t: 'move', x: 782, y: 372, vw: 1024, vh: 640 },
      { ts_ms: t0 + 6_200, t: 'down', x: 782, y: 372, vw: 1024, vh: 640 },
      { ts_ms: t0 + 6_900, t: 'move', x: 936, y: 372, vw: 1024, vh: 640 },
      { ts_ms: t0 + 7_100, t: 'down', x: 936, y: 372, vw: 1024, vh: 640 },
      { ts_ms: t0 + 7_500, t: 'down', x: 936, y: 372, vw: 1024, vh: 640 },
      { ts_ms: t0 + 7_900, t: 'down', x: 936, y: 372, vw: 1024, vh: 640 },
      { ts_ms: t0 + 9_500, t: 'move', x: 880, y: 460, vw: 1024, vh: 640 },
      { ts_ms: t0 + 11_500, t: 'move', x: 845, y: 560, vw: 1024, vh: 640 },
      { ts_ms: t0 + 12_800, t: 'move', x: 700, y: 520, vw: 1024, vh: 640 },
    ],
  }
  const summarize = (r: typeof run) => ({
    id: r.id,
    name: r.name,
    url: r.url,
    session_id: r.session_id,
    project_id: r.project_id,
    card_id: r.card_id,
    started_ms: r.started_ms,
    ended_ms: r.ended_ms,
    step_count: r.steps.length,
    frame_count: r.steps.filter((s) => 'frame' in s && s.frame).length,
    request_count: r.network.length,
    error_count: 3,
  })
  const olderRun = {
    id: 'run-login',
    name: 'login flow — chromium',
    url: `${BASE}/login`,
    session_id: 'ses-demo-2',
    project_id: null,
    card_id: null,
    started_ms: t0 - 39 * 60_000,
    ended_ms: t0 - 39 * 60_000 + 21_400,
    step_count: 9,
    frame_count: 3,
    request_count: 12,
    error_count: 0,
  }

  // The parent-proxied data endpoints the plugin page calls.
  await page.route('**/api/plugin-ui/playwright-video/*', (route) => {
    const url = new URL(route.request().url())
    const json = (body: unknown) =>
      route.fulfill({ contentType: 'application/json', body: JSON.stringify(body) })
    if (url.pathname.endsWith('/runs')) return json({ runs: [summarize(run), olderRun] })
    if (url.pathname.endsWith('/run')) return json({ run })
    if (url.pathname.endsWith('/frame')) {
      return json({ base64: frameShots[url.searchParams.get('frame') ?? ''] ?? '' })
    }
    return route.fulfill({ status: 404, body: '{}' })
  })

  await loadAppAt(page, token, '/plugin-page/playwright-video/playwright-tests')
  const player = page.frameLocator('[data-testid="plugin-fullpage-frame"]')
  await expect(player.locator('.run').first()).toBeVisible({ timeout: 20_000 })
  await expect(player.locator('#player')).toBeVisible({ timeout: 10_000 })
  await expect(player.locator('#frame')).toBeVisible({ timeout: 10_000 })

  // Scrub to ~55% so the stage shows the checkout drawer with the cursor
  // parked on the rage-clicked Apply button.
  const scrub = player.locator('#scrub')
  const box = await scrub.boundingBox()
  expect(box, 'scrub bar rendered').toBeTruthy()
  await scrub.click({ position: { x: box!.width * 0.55, y: Math.max(2, box!.height / 2) } })
  await expect(player.locator('#frame')).toBeVisible()
  // Let the scrubbed frame + cursor overlay settle before capturing.
  await page.waitForTimeout(600)
  await capture(page, 'playwright-player.png')
})

// providers.png — Settings → Providers & Accounts. The page itself is
// real (Ollama/Cursor forms come from the live built-in plugins); the
// account lists and plan usage are stubbed so the shot shows signed-in
// accounts with budgets instead of empty sections.
test('capture providers screenshot @screenshot', async ({ request, page }) => {
  mkdirSync(OUT_DIR, { recursive: true })
  const { token } = await authenticate(request)

  const now = Date.now()
  const hourMs = 3_600_000
  const iso = (ms: number) => new Date(ms).toISOString()

  await page.route('**/api/settings/providers', (route) =>
    route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        providers: [
          { id: 'claude', display_name: 'Claude', hidden: false },
          { id: 'cursor', display_name: 'Cursor', hidden: false },
          { id: 'grok', display_name: 'Grok', hidden: false },
          { id: 'kimi', display_name: 'Kimi Code', hidden: false },
          { id: 'ollama', display_name: 'Ollama', hidden: false },
        ],
      }),
    }),
  )

  const budgetDefaults = {
    config_dir: null,
    budget_window_hours: null,
    budget_limit_usd: null,
    budget_limit_tokens: null,
    warn_threshold: 0.75,
    critical_threshold: 0.9,
  }
  await page.route('**/api/claude-accounts', (route) =>
    route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify([
        {
          id: 'acct-personal',
          name: 'Personal',
          kind: 'oauth_token',
          credential_hint: 'sk-ant-oat…k3Qa',
          ...budgetDefaults,
          created_at: now - 40 * 24 * hourMs,
          updated_at: now - 2 * hourMs,
          usage: {
            total_tokens: 48_200_000,
            est_cost_usd: 96.4,
            turns: 512,
            used_fraction: null,
            level: 'none',
          },
        },
        {
          id: 'acct-team',
          name: 'Team API',
          kind: 'api_key',
          credential_hint: 'sk-ant-api03…9fXe',
          ...budgetDefaults,
          budget_window_hours: 24,
          budget_limit_usd: 40,
          created_at: now - 12 * 24 * hourMs,
          updated_at: now - 5 * hourMs,
          usage: {
            total_tokens: 9_800_000,
            est_cost_usd: 24.6,
            turns: 131,
            used_fraction: 0.61,
            level: 'ok',
          },
        },
      ]),
    }),
  )
  await page.route('**/api/claude-accounts/plan-usage', (route) =>
    route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        default: {
          usage: {
            five_hour: { utilization: 34, resets_at: iso(now + 2.6 * hourMs) },
            seven_day: { utilization: 58, resets_at: iso(now + 77 * hourMs) },
            seven_day_opus: { utilization: 41, resets_at: iso(now + 77 * hourMs) },
            seven_day_sonnet: null,
          },
          fetched_at: now - 11 * 60_000,
          last_error: null,
        },
        'acct-personal': {
          usage: {
            five_hour: { utilization: 12, resets_at: iso(now + 3.1 * hourMs) },
            seven_day: { utilization: 23, resets_at: iso(now + 101 * hourMs) },
            seven_day_opus: null,
            seven_day_sonnet: null,
          },
          fetched_at: now - 11 * 60_000,
          last_error: null,
        },
      }),
    }),
  )
  await page.route('**/api/grok-accounts', (route) =>
    route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify([
        {
          id: 'grok-main',
          name: 'Main',
          kind: 'device',
          authenticated: true,
          ...budgetDefaults,
          created_at: now - 20 * 24 * hourMs,
          updated_at: now - 26 * hourMs,
          usage: {
            total_tokens: 3_100_000,
            est_cost_usd: 6.2,
            turns: 44,
            used_fraction: null,
            level: 'none',
          },
        },
      ]),
    }),
  )
  await page.route('**/api/kimi-accounts', (route) =>
    route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify([
        {
          id: 'kimi-lab',
          name: 'Lab',
          kind: 'api_key',
          authenticated: true,
          ...budgetDefaults,
          created_at: now - 6 * 24 * hourMs,
          updated_at: now - 9 * hourMs,
          usage: {
            total_tokens: 1_450_000,
            est_cost_usd: 2.1,
            turns: 19,
            used_fraction: null,
            level: 'none',
          },
        },
      ]),
    }),
  )

  await loadAppAt(page, token, '/settings')
  await page.getByTestId('settings-nav-providers').click()
  await expect(page.getByTestId('claude-accounts-section')).toBeVisible({ timeout: 10_000 })
  await expect(page.getByTestId('acct-row-acct-personal')).toBeVisible()
  await expect(page.getByTestId('acct-row-acct-team')).toBeVisible()
  await capture(page, 'providers.png')
})
