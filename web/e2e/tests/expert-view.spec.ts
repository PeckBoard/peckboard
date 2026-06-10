import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E for the Expert Sessions view (card C9).
 *
 * Experts are long-lived `is_expert = true` sessions and are hidden from
 * the ordinary chat list. There is no HTTP endpoint to create one — they
 * are only produced by the `spin_up_experts` MCP tool — so this test
 * seeds real experts by driving that tool over the loopback `/mcp`
 * JSON-RPC endpoint:
 *
 *   1. Create a folder pointing at a temp dir that contains source files
 *      (so the partitioner has something to slice), and a project on it.
 *   2. Create a plain session and POST it a `mock:echo` message. The
 *      server issues that session an MCP token and writes it to
 *      `<data_dir>/worker-mcp/<session_id>.json` (data_dir is exported by
 *      playwright.config.ts as PECKBOARD_E2E_DATA_DIR).
 *   3. Read the token and call `spin_up_experts` with it. An unscoped
 *      token may target an explicit project_id, so this is allowed.
 *
 * Then it asserts the UI: the Experts view renders the seeded experts
 * grouped under their project with area + summary + boundaries, and the
 * chat session list never shows an expert.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type AuthBundle = { token: string; authHeader: { Authorization: string } }

async function authenticate(request: APIRequestContext): Promise<AuthBundle> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
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

/** Build a temp source tree with a couple of sizeable topic dirs so
 *  `spin_up_experts` produces at least one knowledge expert. */
function makeSourceTree(): string {
  const root = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-experts-src-'))
  // ~40 KB of "code" per topic dir: above SMALL_WINDOW_BYTES/2 so the
  // size-balanced partitioner yields multiple experts deterministically.
  const body = 'pub fn handler() { /* logic */ }\n'.repeat(1400)
  for (const dir of ['auth', 'billing']) {
    mkdirSync(path.join(root, dir), { recursive: true })
    writeFileSync(path.join(root, dir, 'mod.rs'), body)
  }
  return root
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t as string)
  }, token)
  await page.goto(route)
}

test('experts view shows experts grouped by project; chat list hides them', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)

  // ── Seed: folder + project on a real source tree ──
  const srcDir = makeSourceTree()
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-experts', path: srcDir },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: authHeader,
    // mock:echo so the experts' eager-capture dispatch uses the mock
    // provider (no real `claude` CLI) and returns fast.
    data: {
      name: 'Expert Demo',
      folder_id: folder.id,
      model: 'mock:echo',
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string; name: string }

  // ── Seed: a plain session, message it to mint an MCP token ──
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'expert seeder', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  const msgRes = await request.post(`/api/sessions/${session.id}/message`, {
    headers: authHeader,
    data: { text: 'seed', model: 'mock:echo' },
  })
  expect(msgRes.ok(), `seed message failed: ${await msgRes.text()}`).toBeTruthy()

  // ── Seed: drive spin_up_experts over /mcp with the session's token ──
  const mcpToken = readMcpToken(session.id)
  const mcpRes = await request.post('/mcp', {
    headers: { Authorization: `Bearer ${mcpToken}` },
    data: {
      jsonrpc: '2.0',
      id: 1,
      method: 'tools/call',
      params: {
        name: 'spin_up_experts',
        arguments: { project_id: project.id, max_experts: 3 },
      },
    },
  })
  expect(mcpRes.ok(), `spin_up_experts call failed: ${await mcpRes.text()}`).toBeTruthy()
  const mcpJson = (await mcpRes.json()) as {
    result?: { content?: { text: string }[] }
    error?: { message: string }
  }
  expect(mcpJson.error, `spin_up_experts error: ${JSON.stringify(mcpJson.error)}`).toBeFalsy()
  const toolResult = JSON.parse(mcpJson.result!.content![0].text) as {
    count: number
    experts: { area: string; scope_path: string }[]
  }
  expect(toolResult.count, 'at least one expert spun up').toBeGreaterThan(0)

  // The API now exposes the experts; capture their names/areas for the
  // chat-list exclusion assertion below.
  const expertsRes = await request.get('/api/experts', { headers: authHeader })
  expect(expertsRes.ok()).toBeTruthy()
  const experts = (await expertsRes.json()) as {
    id: string
    name: string
    expert_kind: 'knowledge' | 'question' | null
    knowledge_area: string | null
    knowledge_summary: string | null
    scope_path: string | null
    project_id: string | null
  }[]
  expect(experts.length).toBeGreaterThan(0)
  // The spun-up project carries knowledge-experts plus its question-expert,
  // and the global question-expert exists from boot — assert the view
  // renders both kinds, not just knowledge.
  expect(
    experts.some((e) => e.expert_kind === 'knowledge'),
    'a knowledge expert exists',
  ).toBeTruthy()
  expect(
    experts.some((e) => e.expert_kind === 'question'),
    'a question expert exists',
  ).toBeTruthy()

  // ── UI: the Experts view renders them grouped under the project ──
  await loadAt(page, token, '/experts')

  // The experts are grouped under a section headed by the project name.
  const projectSection = page
    .getByTestId('expert-group')
    .filter({ has: page.getByRole('heading', { name: project.name }) })
  await expect(projectSection).toBeVisible()

  const rows = page.getByTestId('expert-row')
  await expect(rows).toHaveCount(experts.length)

  // Each seeded expert renders (in any order) with its area, a knowledge
  // summary, its boundaries (scope_path), and a Knowledge badge. Match
  // rows by area rather than position — capture runs reshuffle the
  // last_activity ordering between the API read and the UI fetch.
  for (const e of experts) {
    const expectedBadge =
      e.expert_kind === 'question'
        ? 'Question'
        : e.expert_kind === 'knowledge'
          ? 'Knowledge'
          : 'Expert'
    const row = rows.filter({ hasText: e.knowledge_area ?? '' }).first()
    await expect(row).toBeVisible()
    await expect(row.locator('.expert-summary')).not.toBeEmpty()
    await expect(row).toContainText('Boundaries:')
    await expect(row.locator('.expert-scope')).toContainText(e.scope_path ?? '')
    await expect(row.locator('.expert-kind-badge')).toContainText(expectedBadge)
  }

  // ── UI: the chat session list must NOT show any expert ──
  await page.goto('/')
  // The plain seeder session is a normal chat session and should appear.
  await expect(page.locator('.list-view-name', { hasText: 'expert seeder' })).toBeVisible()
  // No expert row chrome and no expert name leaks into the chat list.
  await expect(page.getByTestId('expert-row')).toHaveCount(0)
  for (const e of experts) {
    await expect(page.locator('.list-view-name', { hasText: e.name })).toHaveCount(0)
  }
})
