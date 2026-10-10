import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { execFileSync } from 'node:child_process'
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { expectScreen, spawnSshd, which } from '../tests/ssh-harness'

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
 *   - project.png           — the projects overview list
 *   - plugin-registry.png   — Settings → Plugin Registry browse (real registry.json)
 *   - playwright-player.png — the Playwright Tests replay player mid-run
 *   - providers.png         — Settings → Providers & Accounts with accounts
 *   - subagent-panes.png    — a session with running subagents tiled in split panes
 *   - background-tasks.png  — the Background tasks panel over a chat session
 *   - voice-assistant.png   — the Voice Assistant panel over the sessions list
 *   - dashboard.png         — a saved View as a widget dashboard (project, session, quality)
 *   - terminal.png          — a View with two SSH terminal panes beside a session pane
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

/** Temp source tree so the seeded project's folder holds real code. */
function makeSourceTree(): string {
  const root = mkdtempSync(path.join(tmpdir(), 'peckboard-shots-src-'))
  const body = 'pub fn handler() { /* logic */ }\n'.repeat(1400)
  for (const dir of ['api', 'frontend']) {
    mkdirSync(path.join(root, dir), { recursive: true })
    writeFileSync(path.join(root, dir, 'mod.rs'), body)
  }
  return root
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
    // mutates their step mid-capture; mock:echo so any dispatch stays on
    // the mock provider.
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
 *  test-runner's TS loader refuses to compile). Read lazily so the other
 *  captures still load when that sibling checkout is absent. */
const playwrightTestsPage = (): string => {
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
}

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
        folder_items: [],
      }),
    }),
  )

  // The iframe src — the real plugin page html.
  await page.route('**/plugin-api/v1/playwright-video', (route) =>
    route.fulfill({ contentType: 'text/html; charset=utf-8', body: playwrightTestsPage() }),
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

// document-review.png — the review screen in its annotating state: a real
// markdown document in the doc pane, open annotations of several kinds in
// the rail. All seeded through the live API (no route stubs); the pass is
// never run, so no mock-reviewer text appears in the shot.
test('capture document review screenshot @screenshot', async ({ request, page }) => {
  mkdirSync(OUT_DIR, { recursive: true })
  const { token, authHeader } = await authenticate(request)

  const dir = mkdtempSync(path.join(tmpdir(), 'peckboard-shots-review-'))
  mkdirSync(path.join(dir, 'docs'), { recursive: true })
  const doc = [
    '# Deploy Runbook',
    '',
    'This runbook covers a routine production deploy of the payments',
    'service, from merge to the post-deploy checks.',
    '',
    '## Before You Deploy',
    '',
    'Confirm CI is green on `main` and that the staging environment has',
    'run the release candidate for at least one hour without alerts.',
    '',
    'Announce the deploy in the on-call channel with the release tag.',
    '',
    '## Rolling Out',
    '',
    'Deploys go region by region. Start with `eu-west-1`, watch the error',
    'budget for ten minutes, then continue to the remaining regions.',
    '',
    'If error rates rise above the alert threshold, roll back first and',
    'investigate second.',
    '',
    '## After the Deploy',
    '',
    'Verify the payment success rate on the dashboard and close the',
    'deploy announcement with a link to the release notes.',
    '',
  ].join('\n')
  writeFileSync(path.join(dir, 'docs', 'deploy-runbook.md'), doc)
  const folderId = await createFolder(request, authHeader, 'payments-runbooks', dir)

  const create = await request.post('/api/doc-reviews', {
    headers: authHeader,
    data: { source_kind: 'file', source_ref: `${folderId}:docs/deploy-runbook.md` },
  })
  expect(create.ok(), `create review failed: ${await create.text()}`).toBeTruthy()
  const reviewId = ((await create.json()) as { review: { id: string } }).review.id

  const annotations: Array<{
    start_line: number
    end_line?: number
    quote?: string
    kind: string
    body: string
  }> = [
    {
      start_line: 9,
      quote: 'run the release candidate for at least one hour without alerts',
      kind: 'suggest',
      body: 'Name the alert dashboards to watch — "without alerts" is doing a lot of work here.',
    },
    {
      start_line: 16,
      end_line: 17,
      quote: 'Start with `eu-west-1`',
      kind: 'wrong',
      body: 'We start with us-east-2 since the traffic split changed in Q2.',
    },
    {
      start_line: 23,
      end_line: 25,
      kind: 'expand',
      body: 'Add the rollback command and where to find the previous release tag.',
    },
  ]
  for (const a of annotations) {
    const res = await request.post(`/api/doc-reviews/${reviewId}/comments`, {
      headers: authHeader,
      data: a,
    })
    expect(res.ok(), `annotate failed: ${await res.text()}`).toBeTruthy()
  }

  await loadAppAt(page, token, `/review/${reviewId}`)
  await expect(page.getByTestId('review-view')).toBeVisible({ timeout: 15_000 })
  await expect(page.getByTestId('review-annotation-item')).toHaveCount(3, { timeout: 10_000 })
  await expect(page.getByTestId('review-run-pass')).toBeVisible()
  await capture(page, 'document-review.png')
})

/** A ```mcp block that `mock:mcp` runs against the real MCP handler. */
function mcpBlock(tool: string, args: Record<string, unknown>): string {
  return '```mcp\n' + JSON.stringify({ tool, args }) + '\n```'
}

/** `run_background` shares `run_command`'s approval gate; flip the
 *  host-wide bypass on for the capture and restore it after. */
async function withBypass(
  request: APIRequestContext,
  authHeader: AuthBundle['authHeader'],
  body: () => Promise<void>,
): Promise<void> {
  const prior = await request.get('/api/settings/tool-permissions', { headers: authHeader })
  expect(prior.ok()).toBeTruthy()
  const { bypass } = (await prior.json()) as { bypass: boolean }
  await request.put('/api/settings/tool-permissions', {
    headers: authHeader,
    data: { bypass: true },
  })
  try {
    await body()
  } finally {
    await request.put('/api/settings/tool-permissions', { headers: authHeader, data: { bypass } })
  }
}
type ShotEvent = { seq: number; kind: string; data: { text?: string; source?: string } }

/** Serve each session's event backfill through `edit`, so mock-provider
 *  scaffolding (```mcp JSON, `sleep:N` markers, mock status lines) reads
 *  like a real transcript. Takes effect on the next page load. */
async function polishEvents(
  page: Page,
  edit: (sessionId: string, events: ShotEvent[]) => ShotEvent[],
): Promise<void> {
  await page.route('**/api/sessions/*/events**', async (route) => {
    const res = await route.fetch()
    const body = (await res.json()) as unknown
    const sessionId = new URL(route.request().url()).pathname.split('/')[3]
    const json = Array.isArray(body) ? edit(sessionId, body as ShotEvent[]) : body
    await route.fulfill({ response: res, json })
  })
}

/** Replace an event's text in place. */
function withText(e: ShotEvent, text: string): ShotEvent {
  return { ...e, data: { ...e.data, text } }
}

// subagent-panes.png — a parent session that spawned three real
// spawn_subagent children, each mid-turn (`mock:slow`), so Auto mode
// tiles a live pane per child beside the parent chat.
test('capture subagent panes screenshot @screenshot', async ({ request, page }) => {
  test.setTimeout(90_000)
  mkdirSync(OUT_DIR, { recursive: true })
  const { token, authHeader } = await authenticate(request)
  const folder = await createFolder(
    request,
    authHeader,
    'payments-service',
    mkdtempSync(path.join(tmpdir(), 'peckboard-shots-subagents-')),
  )
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'Harden WebSocket reconnect', folder_id: folder, model: 'mock:mcp' },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const parent = ((await sessionRes.json()) as { id: string }).id

  await loadAppAt(page, token, `/sessions/${parent}`)
  await expect(page.getByTestId('session-workspace')).toBeVisible({ timeout: 15_000 })

  const tasks: [name: string, prompt: string, status: string][] = [
    [
      'Trace reconnect backoff',
      'Trace the reconnect backoff in ws/client.ts.',
      'Reading ws/client.ts — the backoff resets on every heartbeat, not on a successful open.',
    ],
    [
      'Scan staging logs',
      'Scan the staging logs for dropped sockets.',
      'Grepping 4.2k staging log lines for close code 1006 and grouping by client build.',
    ],
    [
      'Draft regression test',
      'Draft a regression test for the reconnect path.',
      'Writing a test that kills the socket mid-stream and asserts one reconnect within 2s.',
    ],
  ]
  const intro = 'Split the reconnect investigation across three subagents.'
  const spawn = await request.post(`/api/sessions/${parent}/message`, {
    headers: authHeader,
    data: {
      text: [
        intro,
        ...tasks.map(([name, prompt]) =>
          mcpBlock('spawn_subagent', { name, prompt, model: 'mock:slow' }),
        ),
      ].join('\n'),
      model: 'mock:mcp',
    },
  })
  expect(spawn.ok(), `spawn failed: ${await spawn.text()}`).toBeTruthy()

  let kids: { id: string; name: string }[] = []
  await expect
    .poll(
      async () => {
        const res = await request.get(`/api/sessions/${parent}/children`, { headers: authHeader })
        const body = (await res.json()) as { id: string; name: string }[] | { children?: [] }
        kids = Array.isArray(body) ? body : (body.children ?? [])
        return kids.length
      },
      { timeout: 20_000 },
    )
    .toBe(tasks.length)
  // The provider-side spawn does not drive the child's first turn; send it.
  const kidTask = new Map<string, (typeof tasks)[number]>()
  for (const task of tasks) {
    const kid = kids.find((k) => k.name.endsWith(task[0]))
    expect(kid, `child ${task[0]}`).toBeTruthy()
    kidTask.set(kid!.id, task)
    await request.post(`/api/sessions/${kid!.id}/message`, {
      headers: authHeader,
      data: { text: `${task[1]} sleep:30`, model: 'mock:slow' },
    })
  }

  const workspace = page.getByTestId('session-workspace')
  await expect(workspace.getByTestId('split-pane')).toHaveCount(tasks.length + 1, {
    timeout: 15_000,
  })

  // Reload with the backfill polished: the parent's prompt without its
  // ```mcp blocks, each child's own task and a plausible status line.
  await polishEvents(page, (sessionId, events) =>
    events.map((e) => {
      const task = kidTask.get(sessionId)
      if (e.kind === 'user' && !e.data.source) {
        return withText(e, task ? task[1] : intro)
      }
      if (e.kind === 'agent-text' && task) return withText(e, task[2])
      if (e.kind === 'agent-text' && e.data.text?.startsWith('ran ')) {
        return withText(e, "Three subagents are on it — I'll merge their findings as they report.")
      }
      return e
    }),
  )
  await page.reload()
  await expect(workspace.getByTestId('split-pane')).toHaveCount(tasks.length + 1, {
    timeout: 15_000,
  })
  for (const [id, task] of kidTask) {
    await expect(workspace.locator(`[data-pane-id="${id}"]`)).toContainText(task[2], {
      timeout: 10_000,
    })
  }
  // Let the slide-in animation settle.
  await page.waitForTimeout(1_000)
  await capture(page, 'subagent-panes.png')
  await page.unrouteAll({ behavior: 'ignoreErrors' })
  // Drop the session so its tab doesn't show up in later captures.
  await request.delete(`/api/sessions/${parent}`, { headers: authHeader })
})

// background-tasks.png — a session that started three real background
// processes through run_background (`mock:mcp`): a finished test run, a
// failed lint, and a dev server still running, with the Background tasks
// panel open on the dev server's output.
test('capture background tasks screenshot @screenshot', async ({ request, page }) => {
  test.setTimeout(90_000)
  mkdirSync(OUT_DIR, { recursive: true })
  const { token, authHeader } = await authenticate(request)
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-shots-bg-'))
  // Real scripts in the folder, so each task's command line reads like a
  // project's own tooling (`sh scripts/dev.sh`) rather than inline shell.
  const scripts: Record<string, string> = {
    'test.sh': [
      'echo "running 142 tests"',
      'echo "test checkout::totals ... ok"',
      'echo "test checkout::coupons ... ok"',
      'echo "test result: ok. 142 passed; 0 failed"',
    ].join('\n'),
    'lint.sh': [
      'echo "src/checkout/Summary.tsx"',
      `echo "  41:7  error  'total' is assigned but never used"`,
      'echo "1 problem (1 error, 0 warnings)"',
      'exit 1',
    ].join('\n'),
    'dev.sh': [
      'echo "  VITE v6.2.0  ready in 312 ms"',
      'echo',
      'echo "  ➜  Local:   http://localhost:5173/"',
      'echo "  ➜  Network: use --host to expose"',
      'echo "12:04:31 [vite] hmr update /src/checkout/Summary.tsx"',
      'sleep 60',
    ].join('\n'),
  }
  mkdirSync(path.join(folderPath, 'scripts'))
  for (const [file, body] of Object.entries(scripts)) {
    writeFileSync(path.join(folderPath, 'scripts', file), body + '\n')
  }
  const folder = await createFolder(request, authHeader, 'storefront', folderPath)
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    // Pinned: completion reports wake the session on its own model.
    data: { name: 'Checkout page redesign', folder_id: folder, model: 'mock:mcp' },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const sessionId = ((await sessionRes.json()) as { id: string }).id

  await withBypass(request, authHeader, async () => {
    await loadAppAt(page, token, `/sessions/${sessionId}`)
    await expect(page.getByTestId('session-workspace')).toBeVisible({ timeout: 15_000 })

    const reason = 'Keep long-running jobs under peckboard'
    const intro = 'Start the dev server, and run the tests and the linter in the background.'
    const job = (label: string, file: string) =>
      mcpBlock('run_background', { command: 'sh', args: [`scripts/${file}`], label, reason })
    const prompt = [
      intro,
      job('unit tests', 'test.sh'),
      job('lint', 'lint.sh'),
      job('dev server', 'dev.sh'),
    ].join('\n')
    const sendRes = await request.post(`/api/sessions/${sessionId}/message`, {
      headers: authHeader,
      data: { text: prompt, model: 'mock:mcp' },
    })
    expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

    await expect(
      page.locator('[data-testid="chat-bg-notice"][data-status="succeeded"]'),
    ).toBeVisible({ timeout: 20_000 })
    await expect(page.locator('[data-testid="chat-bg-notice"][data-status="failed"]')).toBeVisible({
      timeout: 20_000,
    })
    // Both reports wake the session; let those mock turns finish.
    await waitForAgentEnds(request, authHeader, sessionId, 3)

    // Reload with the backfill polished: the prompt without its ```mcp
    // blocks, and the mock's replies to the wake-ups ("no ```mcp blocks")
    // dropped so the completion notices sit under the launch turn.
    await polishEvents(page, (_id, events) => {
      const firstEnd = events.find((e) => e.kind === 'agent-end')?.seq ?? Infinity
      return events
        .filter(
          (e) => e.seq <= firstEnd || !['agent-start', 'agent-end', 'agent-text'].includes(e.kind),
        )
        .map((e) => {
          if (e.kind === 'user' && !e.data.source) return withText(e, intro)
          if (e.kind === 'agent-text' && e.data.text?.startsWith('ran ')) {
            return withText(
              e,
              "All three are running in the background — I'll pick up each result as it lands.",
            )
          }
          return e
        })
    })
    await page.reload()
    const toggle = page.getByTestId('bg-tasks-toggle')
    await expect(toggle).toHaveAttribute('data-running', '1', { timeout: 15_000 })
    await toggle.click()
    const panel = page.getByTestId('bg-tasks-panel')
    await expect(panel.getByTestId('bg-task-row')).toHaveCount(3)
    await panel.getByTestId('bg-task-row').filter({ hasText: 'dev server' }).click()
    await expect(panel.getByTestId('bg-task-output-pre')).toContainText('ready in 312 ms')
    await page.waitForTimeout(500)
    await capture(page, 'background-tasks.png')

    // Stop the dev server so no process outlives the capture.
    await panel.getByTestId('bg-task-output').getByTestId('bg-task-stop').click()
    await page.getByTestId('bg-task-stop-confirm').getByTestId('confirm-dialog-confirm').click()
  })
})

// voice-assistant.png — the Voice Assistant panel over the sessions list,
// mid-conversation: a status question, a question relayed from a worker,
// and the user's answer. The transcript is a scripted backfill for the
// voice session (no model runs), speech recognition is stubbed so the
// panel opens in headless Chromium, and the Kokoro status reads "ready"
// so the first-use download notice (downloads are off in e2e) stays out.
test('capture voice assistant screenshot @screenshot', async ({ request, page }) => {
  mkdirSync(OUT_DIR, { recursive: true })
  const { token, authHeader } = await authenticate(request)
  const folder = await createFolder(
    request,
    authHeader,
    'storefront',
    mkdtempSync(path.join(tmpdir(), 'peckboard-shots-voice-')),
  )
  const sessionIds = new Set<string>()
  for (const name of [
    'Checkout retry logic',
    'Changelog for the next release',
    'Fix cart rounding',
    'Storefront search facets',
  ]) {
    const res = await request.post('/api/sessions', {
      headers: authHeader,
      data: { name, folder_id: folder },
    })
    expect(res.ok(), `create session ${name} failed: ${await res.text()}`).toBeTruthy()
    sessionIds.add(((await res.json()) as { id: string }).id)
  }
  const voiceRes = await request.post('/api/voice/session', {
    headers: authHeader,
    data: { model: 'mock:echo' },
  })
  expect(voiceRes.ok(), `voice session failed: ${await voiceRes.text()}`).toBeTruthy()
  const voiceId = ((await voiceRes.json()) as { session_id: string }).session_id

  // Only this shot's sessions in the list behind the panel, whatever the
  // other captures seeded on the shared server.
  await page.route(
    (url) => url.pathname === '/api/sessions',
    async (route) => {
      const res = await route.fetch()
      const body = (await res.json()) as { items?: { id: string }[] }
      if (route.request().method() !== 'GET' || !Array.isArray(body.items)) {
        return route.fulfill({ response: res, json: body })
      }
      const items = body.items.filter((s) => sessionIds.has(s.id))
      await route.fulfill({ response: res, json: { ...body, items, next_cursor: null } })
    },
  )

  const now = Date.now()
  const script: [kind: string, text: string][] = [
    ['user', "What's running right now?"],
    [
      'agent-text',
      'Three sessions are busy: the checkout worker, the changelog, and Fix cart rounding, which is waiting on your review.',
    ],
    ['agent-end', ''],
    ['user', '[relay] The checkout worker asks: should the retry limit be three or five attempts?'],
    ['agent-text', 'The checkout worker wants to know: three retries or five?'],
    ['agent-end', ''],
    ['user', 'Three. Tell it to log each retry.'],
    ['agent-text', 'Sent to the checkout worker.'],
    ['agent-end', ''],
  ]
  const events = script.map(([kind, text], i) => ({
    id: `voice-shot-${i + 1}`,
    session_id: voiceId,
    seq: i + 1,
    kind,
    data: { text },
    ts: now - (script.length - i) * 15_000,
  }))
  await page.route(`**/api/sessions/${voiceId}/events**`, (route) =>
    route.fulfill({ json: events }),
  )
  const ready = { state: 'ready', progress: 1, error: null }
  await page.route('**/api/voice/tts/prepare', (route) => route.fulfill({ json: ready }))
  await page.route('**/api/voice/tts/status', (route) => route.fulfill({ json: ready }))
  await page.addInitScript(() => {
    class FakeRecognition {
      lang = ''
      continuous = false
      interimResults = false
      maxAlternatives = 1
      onresult = null
      onend = null
      onerror = null
      start() {}
      stop() {}
      abort() {}
    }
    const w = window as unknown as Record<string, unknown>
    w.SpeechRecognition = FakeRecognition
    w.webkitSpeechRecognition = FakeRecognition
  })

  await loadAppAt(page, token, '/')
  const fab = page.getByTestId('voice-fab')
  await expect(fab).toBeVisible({ timeout: 15_000 })
  await fab.click()
  const panel = page.getByTestId('voice-panel')
  await expect(panel).toContainText('three retries or five')
  await expect(page.getByTestId('voice-status')).toHaveText('Listening')
  await expect(page.getByText('Storefront search facets')).toBeVisible()
  await page.waitForTimeout(800)
  await capture(page, 'voice-assistant.png')
})
/** The closing line `polishHappyPath` gives a `mock:happy-path` run. */
const RETRY_DONE = 'Retries are in, with tests for the backoff schedule and the give-up path.'

/** Serve a `mock:happy-path` run as a believable turn: `prompt` as the
 *  user message, and the mock's model chip, placeholder text and `echo
 *  hello` shell call rewritten into a capture-retry change. Next page load. */
async function polishHappyPath(page: Page, prompt: string): Promise<void> {
  const swaps: [from: string, to: string][] = [
    [
      'Working on it...',
      'Captures that fail with a retryable decline now go through RetryPolicy: three attempts, backing off 2s, 8s, then 32s.',
    ],
    ['Say hello to prove the shell works.', 'Run the retry tests'],
    ['mock:happy-path', 'claude:claude-sonnet-5'],
    ['echo hello', 'cargo test retry'],
    ['"hello"', '"test result: ok. 4 passed; 0 failed"'],
    ['"Done."', `"${RETRY_DONE}"`],
  ]
  await polishEvents(page, (_id, events) =>
    events.map((e) => {
      if (e.kind === 'user' && !e.data.source) return withText(e, prompt)
      let json = JSON.stringify(e)
      for (const [from, to] of swaps) json = json.replaceAll(from, to)
      return JSON.parse(json) as ShotEvent
    }),
  )
}

// dashboard.png — a saved View as a widget dashboard: a Project summary of
// a seeded board, a Session pane with a completed mock run, Needs
// Attention, Review Quality, and the Worker Fleet. The project and session
// are real; the read models that only fill up after days of worker
// activity (`/api/dashboard/*`, the summary's active cards and spend) are
// stubbed with realistic data so no widget sits in its empty state.
test('capture dashboard view screenshot @screenshot', async ({ request, page }) => {
  test.setTimeout(90_000)
  mkdirSync(OUT_DIR, { recursive: true })
  const { token, authHeader } = await authenticate(request)
  const folder = await createFolder(
    request,
    authHeader,
    'checkout-api',
    mkdtempSync(path.join(tmpdir(), 'peckboard-shots-dash-')),
  )
  const projectId = await createProject(request, authHeader, 'Checkout API', folder)
  const cards: [title: string, step: string][] = [
    ['Idempotency keys for payment retries', 'backlog'],
    ['Rate-limit coupon redemption', 'backlog'],
    ['Currency rounding in cart totals', 'backlog'],
    ['Webhook signature verification', 'backlog'],
    ['Split tax service out of checkout', 'in_progress'],
    ['Retry failed captures with backoff', 'in_progress'],
    ['Saved cards for returning shoppers', 'in_progress'],
    ['Audit log for refunds', 'review'],
    ['Apple Pay on the payment sheet', 'review'],
    ['Order confirmation emails', 'done'],
    ['Address autocomplete', 'done'],
    ['Guest checkout', 'done'],
    ['Stripe SDK upgrade', 'done'],
  ]
  for (const [i, [title, step]] of cards.entries()) {
    await createCard(request, authHeader, projectId, title, '', i % 3, step)
  }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'Retry failed captures', folder_id: folder },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const sessionId = ((await sessionRes.json()) as { id: string }).id
  const prompt = 'Failed card captures are never retried — add a retry with exponential backoff.'
  const sent = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: prompt, model: 'mock:happy-path' },
  })
  expect(sent.ok(), `message failed: ${await sent.text()}`).toBeTruthy()
  await waitForAgentEnds(request, authHeader, sessionId, 1)

  const ago = (min: number) => new Date(Date.now() - min * 60_000).toISOString()
  const active = [
    ['Split tax service out of checkout', 'in_progress', true],
    ['Retry failed captures with backoff', 'in_progress', true],
    ['Saved cards for returning shoppers', 'in_progress', true],
    ['Audit log for refunds', 'review', false],
  ] as const
  await page.route(`**/api/projects/${projectId}/summary`, async (route) => {
    const res = await route.fetch()
    const body = (await res.json()) as Record<string, unknown>
    await route.fulfill({
      response: res,
      json: {
        ...body,
        active: active.map(([title, step, running], i) => ({
          card_id: `c-${i}`,
          title,
          step,
          worker_session_id: null,
          session_running: running,
        })),
        worker_count: 3,
        blocked_cards: 1,
        spend_today_usd: 4.82,
        last_activity_at: ago(2),
      },
    })
  })
  const at = (kind: string, title: string, detail: string, card: string, min: number) => ({
    kind,
    project_id: projectId,
    project_name: 'Checkout API',
    card_id: `a-${title}`,
    card_title: card,
    session_id: null,
    title,
    detail,
    at: ago(min),
  })
  await page.route('**/api/dashboard/attention**', (route) =>
    route.fulfill({
      json: {
        items: [
          at(
            'question',
            'Should a declined card be retried?',
            'Retry only soft declines, or every failure?',
            'Retry failed captures with backoff',
            4,
          ),
          at(
            'plan',
            'Plan ready for review',
            'Extract TaxClient behind a trait, then move the rate tables.',
            'Split tax service out of checkout',
            18,
          ),
          at(
            'blocked',
            'Webhook signature verification',
            'Waiting on the signing secret from the payments team',
            'Webhook signature verification',
            95,
          ),
        ],
      },
    }),
  )
  const days = Array.from({ length: 30 }, (_, i) => {
    const d = new Date(Date.now() - (29 - i) * 86_400_000)
    return {
      date: d.toISOString().slice(0, 10),
      pass: [3, 5, 2, 6, 4, 1, 0][i % 7] + (i > 20 ? 2 : 0),
      changes_requested: [1, 0, 2, 1, 0, 1, 0][(i * 3) % 7],
      crashes: i % 11 === 4 ? 1 : 0,
    }
  })
  const sum = (k: 'pass' | 'changes_requested' | 'crashes') => days.reduce((n, d) => n + d[k], 0)
  await page.route('**/api/dashboard/review-quality**', (route) =>
    route.fulfill({
      json: {
        days,
        totals: {
          pass: sum('pass'),
          changes_requested: sum('changes_requested'),
          crashes: sum('crashes'),
          retries: 4,
        },
        by_project: [],
      },
    }),
  )
  const workers = [
    ['Split tax service out of checkout', 'in_progress', 'claude:claude-opus-5-5', 47, 0.62],
    ['Retry failed captures with backoff', 'in_progress', 'claude:claude-sonnet-5', 12, 0.31],
    ['Saved cards for returning shoppers', 'in_progress', 'codex:gpt-5.1-codex', 26, 0.48],
    ['Audit log for refunds', 'review', 'claude:claude-sonnet-5', 0, 0.84],
  ] as const
  await page.route('**/api/dashboard/workers**', (route) =>
    route.fulfill({
      json: {
        workers: workers.map(([title, step, model, min, fill], i) => ({
          session_id: `w-${i}`,
          session_name: title,
          project_id: projectId,
          project_name: 'Checkout API',
          card_id: `c-${i}`,
          card_title: title,
          step,
          model,
          started_at: ago(min),
          running: min > 0,
          last_activity_at: ago(1),
          context_tokens: Math.round(fill * 200_000),
        })),
      },
    }),
  )
  await polishHappyPath(page, prompt)

  const viewRes = await request.post('/api/me/views', {
    headers: authHeader,
    data: {
      name: 'Checkout overview',
      widgets: [
        // 14 rows: fills the 800px viewport without the grid scrolling.
        { id: 'w-project', kind: 'project', x: 0, y: 0, w: 4, h: 7, projectId },
        { id: 'w-attn', kind: 'attention', x: 4, y: 0, w: 4, h: 7 },
        { id: 'w-quality', kind: 'review_quality', x: 8, y: 0, w: 4, h: 7 },
        { id: 'w-session', kind: 'session', x: 0, y: 7, w: 5, h: 7, sessionId },
        { id: 'w-workers', kind: 'workers', x: 5, y: 7, w: 7, h: 7 },
      ],
    },
  })
  expect(viewRes.status(), await viewRes.text()).toBe(201)
  const viewId = ((await viewRes.json()) as { id: string }).id

  await loadAppAt(page, token, `/views/${viewId}`)
  await expect(page.getByTestId('view-editor')).toBeVisible({ timeout: 15_000 })
  const widget = (kind: string) => page.locator(`[data-testid="view-widget"][data-kind="${kind}"]`)
  await expect(widget('project').getByTestId('project-widget-active')).toBeVisible({
    timeout: 15_000,
  })
  await expect(widget('attention').getByTestId('dash-attention-list')).toBeVisible()
  await expect(widget('review_quality').getByTestId('dash-review_quality-list')).toBeVisible()
  await expect(widget('workers').getByTestId('dash-workers-list')).toBeVisible()
  await expect(widget('session').getByText(RETRY_DONE)).toBeVisible({ timeout: 15_000 })
  await expect(page.locator('[aria-busy="true"]')).toHaveCount(0, { timeout: 15_000 })
  await page.waitForTimeout(800)
  await capture(page, 'dashboard.png')
  await page.unrouteAll({ behavior: 'ignoreErrors' })
})

// terminal.png — a View mixing a session pane with two SSH terminal panes
// on one host (ssh-fleet, tmux-backed PTYs), each shell showing real
// output from a throwaway git repo. Uses a local OpenSSH daemon on a free
// port, like tests/view-terminal-panes.spec; skipped where sshd or tmux
// is missing rather than faked.
test('capture ssh terminal screenshot @screenshot', async ({ request, page }) => {
  test.setTimeout(120_000)
  test.skip(!which('tmux'), 'tmux not installed')
  const sshd = await spawnSshd()
  test.skip(!sshd, 'OpenSSH sshd/ssh-keygen not available')
  if (!sshd) return
  mkdirSync(OUT_DIR, { recursive: true })
  const { token, authHeader } = await authenticate(request)

  // A small repo with history, so `git log` and `ls` read like a project.
  const repo = path.join(mkdtempSync(path.join(tmpdir(), 'peckboard-shots-term-')), 'checkout-api')
  mkdirSync(path.join(repo, 'src'), { recursive: true })
  mkdirSync(path.join(repo, 'scripts'))
  const git = (...args: string[]) =>
    execFileSync(
      'git',
      ['-c', 'user.name=Dana Reyes', '-c', 'user.email=dana@example.com', ...args],
      {
        cwd: repo,
        stdio: 'pipe',
      },
    )
  git('init', '-q', '-b', 'main')
  const commits: [file: string, body: string, msg: string][] = [
    ['Cargo.toml', '[package]\nname = "checkout-api"\n', 'Initial checkout service skeleton'],
    ['src/main.rs', 'fn main() {}\n', 'Wire up axum router and health check'],
    ['src/cart.rs', '// cart totals\n', 'Cart totals with per-line tax'],
    ['src/payments.rs', '// captures\n', 'Capture payments through the Stripe client'],
    ['src/coupons.rs', '// coupons\n', 'Coupon redemption with usage limits'],
    ['README.md', '# checkout-api\n', 'Document local setup'],
    ['src/retry.rs', '// backoff\n', 'Retry failed captures with exponential backoff'],
  ]
  for (const [file, body, msg] of commits) {
    writeFileSync(path.join(repo, file), body)
    git('add', '.')
    git('commit', '-q', '-m', msg)
  }
  writeFileSync(
    path.join(repo, 'scripts', 'test.sh'),
    [
      'echo "   Compiling checkout-api v0.4.2"',
      'echo "    Finished test profile in 3.41s"',
      'echo "     Running unittests src/main.rs"',
      'echo',
      'echo "running 6 tests"',
      'for t in cart::totals cart::rounding coupons::limit payments::capture retry::backoff retry::gives_up; do echo "test $t ... ok"; done',
      'echo',
      'echo "test result: ok. 6 passed; 0 failed; finished in 0.04s"',
    ].join('\n') + '\n',
  )

  const opened: string[] = []
  try {
    // addFleetHost's flow, with a host label that reads like a real box.
    await request.post('/api/plugins/ssh-fleet/approval', {
      headers: authHeader,
      data: { decision: 'approve' },
    })
    const label = 'build-01'
    const added = await request.post('/api/plugin-ui/ssh-fleet/hosts', {
      headers: authHeader,
      data: {
        label,
        hostname: '127.0.0.1',
        port: sshd.port,
        username: sshd.user,
        private_key: sshd.privateKey,
      },
    })
    expect(added.ok(), `add host failed: ${await added.text()}`).toBeTruthy()
    const hosts = (await (
      await request.get('/api/terminals/hosts', { headers: authHeader })
    ).json()) as { id: string; label: string }[]
    const hostId = hosts.find((h) => h.label === label)?.id ?? ''
    expect(hostId).not.toBe('')

    const folder = await createFolder(request, authHeader, 'checkout-api-term', repo)
    const sessionRes = await request.post('/api/sessions', {
      headers: authHeader,
      data: { name: 'Retry failed captures', folder_id: folder },
    })
    expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
    const sessionId = ((await sessionRes.json()) as { id: string }).id
    const prompt = 'Run the retry tests and summarise what the backoff covers.'
    const sent = await request.post(`/api/sessions/${sessionId}/message`, {
      headers: authHeader,
      data: { text: prompt, model: 'mock:happy-path' },
    })
    expect(sent.ok(), `message failed: ${await sent.text()}`).toBeTruthy()
    await waitForAgentEnds(request, authHeader, sessionId, 1)
    await polishHappyPath(page, prompt)

    const viewRes = await request.post('/api/me/views', {
      headers: authHeader,
      data: { name: 'Checkout ops', layout: { kind: 'leaf', sessionId } },
    })
    expect(viewRes.ok(), await viewRes.text()).toBeTruthy()
    const viewId = ((await viewRes.json()) as { id: string }).id

    await loadAppAt(page, token, `/views/${viewId}`)
    await expect(page.getByTestId('view-editor')).toBeVisible({ timeout: 15_000 })
    const termPanes = page.getByTestId('view-terminal-pane')
    for (let i = 1; i <= 2; i++) {
      await page.getByTestId('add-widget-button').click()
      await page.getByTestId('view-add-terminal').click()
      await page.getByTestId(`view-add-terminal-host-${hostId}`).click()
      await expect(termPanes).toHaveCount(i, { timeout: 15_000 })
    }
    const [t1, t2] = await termPanes.evaluateAll((els) =>
      els.map((e) => e.getAttribute('data-terminal-id') ?? ''),
    )
    opened.push(t1, t2)

    // Session on the left, the two shells stacked on the right; 13 rows
    // fit the 800px viewport without the grid scrolling.
    await expect(page.getByTestId('view-save-state')).toHaveAttribute('data-state', 'saved', {
      timeout: 10_000,
    })
    const view = (await (
      await request.get(`/api/me/views/${viewId}`, { headers: authHeader })
    ).json()) as { widgets: { id: string; kind: string; terminalId?: string }[] }
    const widgets = view.widgets.map((w) =>
      w.kind === 'session'
        ? { ...w, x: 0, y: 0, w: 5, h: 13 }
        : { ...w, x: 5, y: w.terminalId === t1 ? 0 : 6, w: 7, h: w.terminalId === t1 ? 6 : 7 },
    )
    const put = await request.put(`/api/me/views/${viewId}`, {
      headers: authHeader,
      data: { widgets },
    })
    expect(put.ok(), await put.text()).toBeTruthy()
    await page.reload()
    await expect(termPanes).toHaveCount(2, { timeout: 15_000 })
    for (const id of [t1, t2]) {
      await expect(
        page.locator(`[data-terminal-id="${id}"] [data-testid="terminal-pane"]`),
      ).toHaveAttribute('data-phase', 'live', { timeout: 20_000 })
    }

    const run = async (id: string, lines: string[], done: RegExp) => {
      await page.locator(`[data-terminal-id="${id}"] .xterm`).click()
      await page.keyboard.type(`cd ${repo} && export PS1='\\u@${label}:\\W$ ' && clear\n`)
      await page.waitForTimeout(800)
      for (const line of lines) {
        await page.keyboard.type(`${line}\n`)
        await page.waitForTimeout(500)
      }
      await expectScreen(page, done, id)
    }
    await run(t1, ['git log --oneline --decorate'], /Initial checkout service skeleton/)
    await run(t2, ['ls', 'sh scripts/test.sh'], /6 passed/)
    await expect(page.getByText(RETRY_DONE)).toBeVisible()
    // Drop focus so neither terminal draws an active cursor.
    await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur())
    await page.waitForTimeout(800)
    await capture(page, 'terminal.png')
  } finally {
    await page.unrouteAll({ behavior: 'ignoreErrors' })
    for (const id of opened) {
      await request.delete(`/api/terminals/${id}`, { headers: authHeader }).catch(() => {})
    }
    sshd.child.kill()
  }
})
