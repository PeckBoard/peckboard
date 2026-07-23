import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { existsSync, mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

/**
 * End-to-end tests for the chicken-coop plugin: one 3D hen per card being
 * worked on. The wasm is staged into the data dir by playwright.config.ts
 * and approved by global-setup; tests self-skip when it isn't built.
 *
 * Determinism: workers run mock models. `mock:ask` blocks forever, giving a
 * stable in-progress worker (a hen that stays out); `mock:happy-path` emits
 * a scripted Bash tool event into the event log, which is exactly what the
 * plugin's activity counter tails — no real provider needed.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const WASM_PATH = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  '..',
  '..',
  '..',
  '..',
  'peck-plugins',
  'chicken-coop',
  'dist',
  'plugin.wasm',
)

const FRAME_SEL = '[data-testid="plugin-fullpage-frame"]'

async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

/** Skip when the wasm isn't staged/approved; re-approve if merely pending. */
async function ensurePluginActive(
  request: APIRequestContext,
  auth: Record<string, string>,
): Promise<void> {
  const res = await request.get('/api/plugins', { headers: auth })
  expect(res.ok()).toBeTruthy()
  const catalog = (await res.json()) as {
    wasm_plugins?: Array<{ name: string; status: string }>
  }
  const plugin = (catalog.wasm_plugins ?? []).find((p) => p.name === 'chicken-coop')
  if (!plugin) {
    test.skip(true, 'chicken-coop plugin not loaded — config should stage the wasm')
    return
  }
  if (plugin.status !== 'approved') {
    const approve = await request.post('/api/plugins/chicken-coop/approval', {
      headers: auth,
      data: { decision: 'approve' },
    })
    expect(approve.ok(), `approval failed: ${await approve.text()}`).toBeTruthy()
  }
}

async function makeProject(
  request: APIRequestContext,
  auth: Record<string, string>,
  name: string,
  opts: { workflow: string; model: string; workerCount?: number },
) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-coop-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-coop-${Date.now()}-${name}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name,
      folder_id: folder.id,
      worker_count: opts.workerCount ?? 1,
      workflow: opts.workflow,
      model: opts.model,
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  return (await projectRes.json()) as { id: string }
}

async function makeCard(
  request: APIRequestContext,
  auth: Record<string, string>,
  projectId: string,
  title: string,
) {
  const res = await request.post(`/api/projects/${projectId}/cards`, {
    headers: auth,
    data: { title, description: '', step: 'backlog', priority: 1 },
  })
  expect(res.ok(), `create card failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as { id: string }
}

async function moveCard(
  request: APIRequestContext,
  auth: Record<string, string>,
  projectId: string,
  cardId: string,
  step: string,
) {
  const res = await request.put(`/api/projects/${projectId}/cards/${cardId}`, {
    headers: auth,
    data: { step },
  })
  expect(res.ok(), `move to ${step} failed: ${await res.text()}`).toBeTruthy()
}

/** Poll until the card has a worker_session_id (orchestrator tick ≤ 5s). */
async function waitForWorker(
  request: APIRequestContext,
  auth: Record<string, string>,
  projectId: string,
  cardId: string,
  timeoutMs: number,
): Promise<string> {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    const res = await request.get(`/api/projects/${projectId}/cards`, { headers: auth })
    expect(res.ok()).toBeTruthy()
    const cards = (await res.json()) as Array<{ id: string; worker_session_id: string | null }>
    const card = cards.find((c) => c.id === cardId)
    if (card && typeof card.worker_session_id === 'string') return card.worker_session_id
    await new Promise((r) => setTimeout(r, 500))
  }
  throw new Error(`worker never spawned within ${timeoutMs}ms`)
}

type CoopChicken = {
  card_id: string
  phase: string
  activity: number
  tool_class: string | null
  busy: boolean
  title: string
}

async function coopState(
  request: APIRequestContext,
  auth: Record<string, string>,
): Promise<CoopChicken[]> {
  const res = await request.get('/api/plugin-ui/chicken-coop/state', { headers: auth })
  expect(res.ok(), `state endpoint failed: ${await res.text()}`).toBeTruthy()
  const body = (await res.json()) as { chickens: CoopChicken[] }
  return body.chickens
}

/** Open the app authenticated and navigate to the Chicken Coop page. */
async function openCoopPage(page: Page, token: string) {
  await page.addInitScript((injectedToken: string) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/')
  await page.getByTestId('plugin-sidebar-chicken-coop-chicken-coop').click()
  await expect(page.locator(FRAME_SEL)).toBeVisible({ timeout: 10_000 })
  return page.frameLocator(FRAME_SEL)
}

// Worker orchestration takes several 5s ticks; walking/despawn animations
// add bounded seconds on top. Give the suite explicit headroom.
test.describe.configure({ timeout: 150_000 })

test.beforeEach(() => {
  test.skip(
    !existsSync(WASM_PATH),
    'chicken-coop wasm not built — run peck-plugins/chicken-coop/build.sh',
  )
})

test('coop page is served and listed in the sidebar catalog', async ({ request, baseURL }) => {
  expect(baseURL).toBeTruthy()
  const { auth } = await authenticate(request)
  await ensurePluginActive(request, auth)

  const catalogRes = await request.get('/api/plugins', { headers: auth })
  const catalog = (await catalogRes.json()) as {
    sidebar_items?: Array<{ plugin: string; label: string; path: string }>
  }
  const item = (catalog.sidebar_items ?? []).find((i) => i.plugin === 'chicken-coop')
  expect(item, 'sidebar item registered').toBeTruthy()
  expect(item!.label).toBe('Chicken Coop')
  expect(item!.path).toBe('/plugin-api/v1/chicken-coop')

  const pageRes = await request.get('/plugin-api/v1/chicken-coop')
  expect(pageRes.status()).toBe(200)
  const html = await pageRes.text()
  expect(html).toContain('Chicken Coop')
  expect(html).toContain('coop-mirror')
})

test('hen lifecycle: out of the coop → working → nesting in review → home on done', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL).toBeTruthy()
  const { token, auth } = await authenticate(request)
  await ensurePluginActive(request, auth)

  // deep-develop-software has a real testing step (review) between
  // in_progress and done; mock:ask keeps the worker alive indefinitely.
  const project = await makeProject(request, auth, 'coop-lifecycle', {
    workflow: 'deep-develop-software',
    model: 'mock:ask',
  })
  const card = await makeCard(request, auth, project.id, 'Guard the henhouse')
  await waitForWorker(request, auth, project.id, card.id, 30_000)

  // Backend state: one working hen.
  await expect
    .poll(async () => (await coopState(request, auth)).find((c) => c.card_id === card.id)?.phase, {
      timeout: 15_000,
    })
    .toBe('working')

  // UI: the hen is out in the run.
  const frame = await openCoopPage(page, token)
  const hen = frame.locator(`[data-testid="coop-chicken"][data-card-id="${card.id}"]`)
  await expect(hen).toHaveAttribute('data-phase', 'working', { timeout: 15_000 })
  await expect(frame.getByTestId('coop-hud')).toContainText('working')
  await page.screenshot({ path: test.info().outputPath('1-working.png') })

  // Force a peck through the page's test hook and watch the animation
  // machinery run (real pecks fire on activity deltas between polls, which
  // a blocked mock:ask worker never produces).
  type CoopTestWindow = Window & { __coopTest: { peck: (cardId: string, cls: string) => boolean } }
  const henFound = await frame
    .locator('body')
    .evaluate(
      (_, id) => (window as unknown as CoopTestWindow).__coopTest.peck(id, 'command'),
      card.id,
    )
  expect(henFound).toBe(true)
  await expect
    .poll(async () => Number(await hen.getAttribute('data-pecks').then((v) => v ?? '0')), {
      timeout: 15_000,
    })
    .toBeGreaterThan(0)
  await page.screenshot({ path: test.info().outputPath('2-peck.png') })

  // Review step → the hen nests.
  await moveCard(request, auth, project.id, card.id, 'review')
  await expect(hen).toHaveAttribute('data-phase', 'testing', { timeout: 15_000 })
  // She has to walk to the nest; wait for the sit.
  await expect(hen).toHaveAttribute('data-anim', 'nest', { timeout: 30_000 })
  await page.screenshot({ path: test.info().outputPath('3-testing.png') })

  // Done → she heads home and despawns at the coop door.
  await moveCard(request, auth, project.id, card.id, 'done')
  await expect
    .poll(async () => (await coopState(request, auth)).find((c) => c.card_id === card.id)?.phase, {
      timeout: 15_000,
    })
    .toBe('done')
  await expect(hen).toHaveCount(0, { timeout: 45_000 })
  await page.screenshot({ path: test.info().outputPath('4-gone.png') })
})

test('worker tool activity reaches the hen as pecking fuel', async ({ request, baseURL }) => {
  expect(baseURL).toBeTruthy()
  const { auth } = await authenticate(request)
  await ensurePluginActive(request, auth)

  // mock:happy-path emits a scripted Bash agent-tool-start into the event
  // log and finishes. The plugin tails that log, so activity must land
  // regardless of what step the card ends up on (terminal cards linger in
  // the roster for 2 minutes).
  const project = await makeProject(request, auth, 'coop-pecks', {
    workflow: 'task',
    model: 'mock:happy-path',
  })
  const card = await makeCard(request, auth, project.id, 'Peck at the terminal')
  await waitForWorker(request, auth, project.id, card.id, 30_000)

  await expect
    .poll(
      async () => {
        const hen = (await coopState(request, auth)).find((c) => c.card_id === card.id)
        return hen ? hen.activity : -1
      },
      { timeout: 30_000 },
    )
    .toBeGreaterThan(0)

  const hen = (await coopState(request, auth)).find((c) => c.card_id === card.id)
  expect(hen!.tool_class).toBe('command')
  expect(hen!.title).toBe('Peck at the terminal')

  // Park the card: an instant-completing mock worker otherwise respawns
  // every orchestrator pass for the rest of the suite (needless churn).
  await moveCard(request, auth, project.id, card.id, 'done')
})

test('one hen per card: two active cards → two hens', async ({ request, page, baseURL }) => {
  expect(baseURL).toBeTruthy()
  const { token, auth } = await authenticate(request)
  await ensurePluginActive(request, auth)

  const project = await makeProject(request, auth, 'coop-flock', {
    workflow: 'task',
    model: 'mock:ask',
    workerCount: 2,
  })
  const cardA = await makeCard(request, auth, project.id, 'First hen')
  const cardB = await makeCard(request, auth, project.id, 'Second hen')
  // The orchestrator spawns at most one worker per tick — allow two ticks.
  await waitForWorker(request, auth, project.id, cardA.id, 30_000)
  await waitForWorker(request, auth, project.id, cardB.id, 30_000)

  await expect
    .poll(
      async () => {
        const chickens = await coopState(request, auth)
        return [cardA.id, cardB.id].filter((id) => chickens.some((c) => c.card_id === id)).length
      },
      { timeout: 15_000 },
    )
    .toBe(2)

  const frame = await openCoopPage(page, token)
  // Scope to this test's cards — hens from earlier tests may still be
  // walking home (terminal cards linger briefly in the roster).
  for (const id of [cardA.id, cardB.id]) {
    await expect(
      frame.locator(`[data-testid="coop-chicken"][data-card-id="${id}"]`),
    ).toHaveAttribute('data-phase', 'working', { timeout: 15_000 })
  }
  // Counts in the HUD are global (hens from other tests linger briefly);
  // the per-card asserts above are the real check. Just sanity the HUD.
  await expect(frame.getByTestId('coop-hud')).toContainText('hens out', { timeout: 10_000 })
  await page.screenshot({ path: test.info().outputPath('flock.png') })
})
