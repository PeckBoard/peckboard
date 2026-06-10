import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E for the Expert Sessions backend flows that the C9 view test does not
 * exercise, all driven through the REAL running binary over HTTP + the
 * loopback `/mcp` JSON-RPC endpoint (mock provider, deterministic):
 *
 *   1. Bootstrap — the default GLOBAL question-expert exists on boot, and a
 *      per-project question-expert exists after a project is created.
 *   2. ask_expert — a session asks an expert and the question lands on the
 *      expert session while a context-coupled answer is delivered back to the
 *      caller (the async contract); cross-scope targets are rejected.
 *   3. Q&A export — a resolved user answer is persisted to the rehydration
 *      report files and is readable through the reports API.
 *
 * These complement the in-process integration tests (tests/ask_expert.rs,
 * tests/expert_bootstrap.rs, tests/question_expert_persistence.rs) by proving
 * the same contracts hold through the HTTP/MCP layers a real user hits.
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

type Expert = {
  id: string
  name: string
  expert_kind: 'knowledge' | 'question' | null
  knowledge_area: string | null
  knowledge_summary: string | null
  scope_path: string | null
  project_id: string | null
  is_permanent: boolean
}

async function getExperts(
  request: APIRequestContext,
  authHeader: { Authorization: string },
): Promise<Expert[]> {
  const res = await request.get('/api/experts', { headers: authHeader })
  expect(res.ok(), `GET /api/experts failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as Expert[]
}

type SessionEvent = { kind: string; data: { text?: string } }

async function getEvents(
  request: APIRequestContext,
  authHeader: { Authorization: string },
  sessionId: string,
): Promise<SessionEvent[]> {
  const res = await request.get(`/api/sessions/${sessionId}/events`, { headers: authHeader })
  expect(res.ok(), `GET events failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as SessionEvent[]
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

/** Call an MCP tool over the loopback /mcp JSON-RPC endpoint. */
async function mcpCall(
  request: APIRequestContext,
  mcpToken: string,
  name: string,
  args: Record<string, unknown>,
): Promise<{ result?: Record<string, unknown>; error?: { message: string } }> {
  const res = await request.post('/mcp', {
    headers: { Authorization: `Bearer ${mcpToken}` },
    data: { jsonrpc: '2.0', id: 1, method: 'tools/call', params: { name, arguments: args } },
  })
  expect(res.ok(), `/mcp ${name} HTTP failed: ${await res.text()}`).toBeTruthy()
  const json = (await res.json()) as {
    result?: { content?: { text: string }[] }
    error?: { message: string }
  }
  if (json.error) return { error: json.error }
  const text = json.result!.content![0].text
  return { result: JSON.parse(text) as Record<string, unknown> }
}

function makeSourceTree(): string {
  const root = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-flows-src-'))
  const body = 'pub fn handler() { /* logic */ }\n'.repeat(1400)
  for (const dir of ['auth', 'billing']) {
    mkdirSync(path.join(root, dir), { recursive: true })
    writeFileSync(path.join(root, dir, 'mod.rs'), body)
  }
  return root
}

/** Create a folder on a real temp source tree + a mock-model project. */
async function seedProject(
  request: APIRequestContext,
  authHeader: { Authorization: string },
  name: string,
): Promise<{ projectId: string; folderId: string }> {
  const srcDir = makeSourceTree()
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `${name}-folder`, path: srcDir },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: authHeader,
    data: { name, folder_id: folder.id, model: 'mock:echo', workflow: 'task' },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }
  return { projectId: project.id, folderId: folder.id }
}

test('bootstrap: global question-expert exists on boot; per-project one after project creation', async ({
  request,
}) => {
  const { authHeader } = await authenticate(request)

  // The default global question-expert is created at startup (main.rs) and is
  // permanent with a stable id — it must already be present before any project.
  const onBoot = await getExperts(request, authHeader)
  const global = onBoot.find((e) => e.expert_kind === 'question' && e.project_id === null)
  expect(global, 'global question-expert exists on boot').toBeTruthy()
  expect(global!.is_permanent, 'global question-expert is permanent').toBeTruthy()

  // Creating a project provisions that project its own question-expert.
  const { projectId } = await seedProject(request, authHeader, 'Bootstrap Demo')
  await expect
    .poll(
      async () => {
        const experts = await getExperts(request, authHeader)
        return experts.some((e) => e.expert_kind === 'question' && e.project_id === projectId)
      },
      { message: 'per-project question-expert created after project creation', timeout: 10_000 },
    )
    .toBeTruthy()
})

test('ask_expert delivers the question to the expert and a context-coupled answer to the caller', async ({
  request,
}) => {
  const { authHeader } = await authenticate(request)

  // A plain session whose first message mints an (unscoped) MCP token.
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: {
      name: 'ask-caller-folder',
      path: mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-ask-')),
    },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'ask caller', folder_id: folder.id },
  })
  expect(sessionRes.ok()).toBeTruthy()
  const caller = (await sessionRes.json()) as { id: string }
  const msgRes = await request.post(`/api/sessions/${caller.id}/message`, {
    headers: authHeader,
    data: { text: 'seed', model: 'mock:echo' },
  })
  expect(msgRes.ok(), `seed message failed: ${await msgRes.text()}`).toBeTruthy()
  const callerToken = readMcpToken(caller.id)

  // The global question-expert is reachable by any (even unscoped) caller.
  const experts = await getExperts(request, authHeader)
  const target = experts.find((e) => e.expert_kind === 'question' && e.project_id === null)!
  expect(target, 'global expert reachable').toBeTruthy()

  const question = 'What is the commit message style for this repo?'
  const ask = await mcpCall(request, callerToken, 'ask_expert', {
    expert_id: target.id,
    question,
  })
  expect(ask.error, `ask_expert errored: ${JSON.stringify(ask.error)}`).toBeFalsy()
  expect(ask.result!.status).toBe('ok')
  expect(ask.result!.delivered).toBe(true)

  // The question landed on the expert session as a consultation event.
  const expertEvents = await getEvents(request, authHeader, target.id)
  expect(
    expertEvents.some(
      (e) => (e.data.text ?? '').includes(question) && (e.data.text ?? '').includes('consultation'),
    ),
    `expert must receive the question; got: ${JSON.stringify(expertEvents.map((e) => e.data.text))}`,
  ).toBeTruthy()

  // A context-coupled answer was delivered back to the caller (read on its
  // next turn — the async contract).
  const callerEvents = await getEvents(request, authHeader, caller.id)
  expect(
    callerEvents.some(
      (e) =>
        (e.data.text ?? '').includes('Expert answer') && (e.data.text ?? '').includes(question),
    ),
    `caller must receive a context-coupled answer; got: ${JSON.stringify(callerEvents.map((e) => e.data.text))}`,
  ).toBeTruthy()

  // Scope enforcement: a project-scoped knowledge expert is NOT reachable by
  // this unscoped caller.
  const { projectId } = await seedProject(request, authHeader, 'Scope Demo')
  const spin = await mcpCall(request, callerToken, 'spin_up_experts', {
    project_id: projectId,
    max_experts: 2,
  })
  expect(spin.error, `spin_up_experts errored: ${JSON.stringify(spin.error)}`).toBeFalsy()
  const projExperts = (await getExperts(request, authHeader)).filter(
    (e) => e.project_id === projectId && e.expert_kind === 'knowledge',
  )
  expect(projExperts.length, 'a project knowledge expert was created').toBeGreaterThan(0)

  const denied = await mcpCall(request, callerToken, 'ask_expert', {
    expert_id: projExperts[0].id,
    question: 'leak project internals',
  })
  expect(denied.error, 'unscoped caller must be rejected from a project expert').toBeTruthy()
  expect(denied.error!.message.toLowerCase()).toContain('scope')
})

test('Q&A export: a resolved user answer is persisted to a rehydration report readable via the reports API', async ({
  request,
}) => {
  const { authHeader } = await authenticate(request)

  // A plain (global-scope) chat session. Resolving a question on it feeds the
  // GLOBAL question-expert and persists the global Q&A export.
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'qa-folder', path: mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-qa-')) },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'qa session', folder_id: folder.id, model: 'mock:echo' },
  })
  expect(sessionRes.ok()).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  // Append a question, then resolve it with a user answer.
  const answer = 'Conventional Commits with a Co-Authored-By trailer.'
  const qRes = await request.post(`/api/sessions/${session.id}/events`, {
    headers: authHeader,
    data: { kind: 'question', data: { questions: [{ question: 'What commit style?' }] } },
  })
  expect(qRes.ok(), `append question failed: ${await qRes.text()}`).toBeTruthy()
  const question = (await qRes.json()) as { id: string }

  const rRes = await request.post(`/api/sessions/${session.id}/events`, {
    headers: authHeader,
    data: {
      kind: 'question-resolved',
      data: { question_id: question.id, rejected: false, answers: { '0': answer } },
    },
  })
  expect(rRes.ok(), `append question-resolved failed: ${await rRes.text()}`).toBeTruthy()

  // The feedback + export write runs in a background task; poll the reports
  // listing until the global Q&A export appears.
  let exportReport: { folder: string; file: string } | undefined
  await expect
    .poll(
      async () => {
        const res = await request.get('/api/reports', { headers: authHeader })
        if (!res.ok()) return false
        const { reports } = (await res.json()) as { reports: { folder: string; file: string }[] }
        exportReport = reports.find((r) => r.folder === 'qa-export-global')
        return Boolean(exportReport)
      },
      { message: 'global Q&A export report is produced', timeout: 10_000 },
    )
    .toBeTruthy()

  // And it is readable, carrying the recorded answer.
  const readRes = await request.get(`/api/reports/${exportReport!.folder}/${exportReport!.file}`, {
    headers: authHeader,
  })
  expect(readRes.ok(), `read export failed: ${await readRes.text()}`).toBeTruthy()
  const body = await readRes.text()
  expect(body).toContain(answer)
})
