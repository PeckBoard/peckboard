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
 * real, and writes four PNGs with stable names into
 * `docs/assets/screenshots/` for the public docs site to reference:
 *
 *   - board.png   — per-project kanban with cards across columns
 *   - chat.png    — a chat session with a completed mock agent run
 *   - experts.png — the experts view with knowledge/question/PM experts
 *   - project.png — the projects overview list
 *
 * Re-run with `cd web && npm run screenshots` (after `npm install` and
 * the one-time `npm run e2e:install`). The run is idempotent: it boots
 * on its own ports (4446/4447, so a leftover e2e server on 4444 is
 * never reused), seeds from scratch, and overwrites the four PNGs.
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
    mcpServers: { peckboard: { env: { PECKBOARD_TOKEN: string } } }
  }
  return cfg.mcpServers.peckboard.env.PECKBOARD_TOKEN
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

  // experts.png — the experts view.
  await page.goto('/experts')
  await expect(page.getByTestId('expert-row').first()).toBeVisible({ timeout: 10_000 })
  await capture(page, 'experts.png')
})
