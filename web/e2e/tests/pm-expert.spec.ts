import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync, readFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E for the PM expert Q&A flows.
 *
 * Every project gets a permanent PM expert (`expert_kind = "pm"`) whose
 * window is a FORM, not a chat: pending questions the user must answer,
 * and the recorded decision log. Pending questions are only created by
 * the `pm_escalate_to_user` MCP tool, callable solely by the project's
 * own PM expert session — so each test seeds by:
 *
 *   1. Creating a folder + mock-model project (the PM expert is
 *      provisioned in the background; poll /api/experts for it).
 *   2. Messaging the PM expert session once so the dispatch path mints
 *      its MCP token at `<data_dir>/worker-mcp/<pm_expert_id>.json`.
 *   3. Calling `pm_escalate_to_user` over the loopback `/mcp` JSON-RPC
 *      endpoint with that token.
 *
 * Each test gets a fresh project. Tests that create a pending question
 * resolve it before finishing, because the rail badge aggregates pending
 * counts across ALL projects.
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

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t as string)
  }, token)
  await page.goto(route)
}

type Expert = { id: string; expert_kind: string | null; project_id: string | null; name: string }

type PmSeed = {
  projectId: string
  projectName: string
  pmExpertId: string
  pmToken: string
}

/** Create a project and return its PM expert with a minted MCP token. */
async function seedPmProject(
  request: APIRequestContext,
  authHeader: { Authorization: string },
  name: string,
): Promise<PmSeed> {
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `${name}-folder`, path: mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-pm-')) },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: authHeader,
    data: { name, folder_id: folder.id, model: 'mock:echo', workflow: 'task' },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  // The PM expert is provisioned in the background after project creation.
  let pmExpert: Expert | undefined
  await expect
    .poll(
      async () => {
        const res = await request.get('/api/experts', { headers: authHeader })
        if (!res.ok()) return false
        const experts = (await res.json()) as Expert[]
        pmExpert = experts.find((e) => e.expert_kind === 'pm' && e.project_id === project.id)
        return Boolean(pmExpert)
      },
      { message: 'per-project PM expert created', timeout: 10_000 },
    )
    .toBeTruthy()

  // Messaging the PM expert session dispatches it (mock provider) and
  // writes its worker-mcp token file — the only way to mint the token.
  const msgRes = await request.post(`/api/sessions/${pmExpert!.id}/message`, {
    headers: authHeader,
    data: { text: 'seed', model: 'mock:echo' },
  })
  expect(msgRes.ok(), `seed message failed: ${await msgRes.text()}`).toBeTruthy()

  return {
    projectId: project.id,
    projectName: name,
    pmExpertId: pmExpert!.id,
    pmToken: readMcpToken(pmExpert!.id),
  }
}

/** Park a pending question via pm_escalate_to_user (as the PM expert). */
async function escalate(
  request: APIRequestContext,
  pmToken: string,
  question: string,
): Promise<string> {
  const esc = await mcpCall(request, pmToken, 'pm_escalate_to_user', { question })
  expect(esc.error, `pm_escalate_to_user errored: ${JSON.stringify(esc.error)}`).toBeFalsy()
  expect(esc.result!.status).toBe('ok')
  return esc.result!.pending_question_id as string
}

/** Answer a pending question over the same HTTP route the form uses. */
async function answerViaApi(
  request: APIRequestContext,
  authHeader: { Authorization: string },
  projectId: string,
  questionId: string,
  answer: string,
): Promise<{ id: string }> {
  const res = await request.post(`/api/projects/${projectId}/pm/questions/${questionId}/answer`, {
    headers: authHeader,
    data: { answer },
  })
  expect(res.ok(), `answer question failed: ${await res.text()}`).toBeTruthy()
  const { decision } = (await res.json()) as { decision: { id: string } }
  return decision
}

/** The PM expert row inside this project's group on the /experts view. */
function pmRowFor(page: Page, projectName: string) {
  const group = page
    .getByTestId('expert-group')
    .filter({ has: page.getByRole('heading', { name: projectName }) })
  return group.locator('[data-testid="expert-row"][data-expert-kind="pm"]')
}

test('pending question shows rail badge, row indicator, and renders in the Q&A view', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const seed = await seedPmProject(request, authHeader, 'PM E2E Pending')

  const question = 'Should prices be stored as integer cents?'
  const pendingId = await escalate(request, seed.pmToken, question)

  await loadAt(page, token, '/experts')

  // The rail badge flags pending PM questions from anywhere in the app.
  await expect(page.getByTestId('pm-expert-waiting-badge')).toBeVisible()

  // The PM expert row carries the per-project waiting indicator.
  const row = pmRowFor(page, seed.projectName)
  await expect(row).toBeVisible()
  const indicator = row.getByTestId('pm-expert-waiting-indicator')
  await expect(indicator).toBeVisible()
  await expect(indicator).toContainText('1 question waiting')

  // Opening the expert lands on the Q&A form with the question pending.
  await row.click()
  await expect(page.getByTestId('pm-expert-view')).toBeVisible()
  const pendingRow = page.getByTestId('pm-pending-question')
  await expect(pendingRow).toHaveCount(1)
  await expect(pendingRow).toContainText(question)

  // Resolve the question so the global rail badge resets for later tests.
  await answerViaApi(request, authHeader, seed.projectId, pendingId, 'Yes, integer cents.')
})

test('answering moves the question to Decisions and clears badge/indicator without reload', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const seed = await seedPmProject(request, authHeader, 'PM E2E Answer')

  const question = 'Do we support multi-currency invoices?'
  await escalate(request, seed.pmToken, question)

  await loadAt(page, token, '/experts')
  const row = pmRowFor(page, seed.projectName)
  await row.click()
  await expect(page.getByTestId('pm-expert-view')).toBeVisible()

  const answer = 'No — USD only for the first release.'
  await page.getByTestId('pm-answer-input').fill(answer)
  await page.getByTestId('pm-answer-submit').click()

  // The question leaves the pending section and lands in Decisions.
  await expect(page.getByTestId('pm-pending-question')).toHaveCount(0)
  await expect(page.getByTestId('pm-pending-empty')).toBeVisible()
  const decision = page.getByTestId('pm-decision-row')
  await expect(decision).toHaveCount(1)
  await expect(decision).toContainText(question)
  await expect(decision).toContainText(answer)

  // The rail badge clears live — no reload happened in this test.
  await expect(page.getByTestId('pm-expert-waiting-badge')).toHaveCount(0)

  // Back on the experts list, the row indicator is gone too.
  await page.getByRole('button', { name: '← Back' }).click()
  const backRow = pmRowFor(page, seed.projectName)
  await expect(backRow).toBeVisible()
  await expect(backRow.getByTestId('pm-expert-waiting-indicator')).toHaveCount(0)
})

test('editing a decision updates the text and persists across a reload', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const seed = await seedPmProject(request, authHeader, 'PM E2E Edit')

  const question = 'Which date format do exports use?'
  const pendingId = await escalate(request, seed.pmToken, question)
  await answerViaApi(request, authHeader, seed.projectId, pendingId, 'ISO 8601.')

  await loadAt(page, token, '/experts')
  await pmRowFor(page, seed.projectName).click()
  await expect(page.getByTestId('pm-expert-view')).toBeVisible()

  const decision = page.getByTestId('pm-decision-row')
  await expect(decision).toContainText('ISO 8601.')

  // Edit the decision; saving requires an explicit confirmation.
  await decision.getByTestId('pm-decision-edit').click()
  const edited = 'ISO 8601 in UTC, always with an explicit offset.'
  await decision.getByTestId('pm-decision-edit-answer').fill(edited)
  await decision.getByTestId('pm-decision-edit-save').click()
  await page.getByRole('button', { name: 'Change decision' }).click()

  // The superseding decision replaces the old row in place.
  await expect(page.getByTestId('pm-decision-edit-form')).toHaveCount(0)
  await expect(page.getByTestId('pm-decision-row')).toHaveCount(1)
  await expect(page.getByTestId('pm-decision-row')).toContainText(edited)

  // And it persists: a full reload lands back on the form with the edit.
  await page.reload()
  await expect(page.getByTestId('pm-expert-view')).toBeVisible()
  await expect(page.getByTestId('pm-decision-row')).toHaveCount(1)
  await expect(page.getByTestId('pm-decision-row')).toContainText(edited)
})

test('the PM expert view is a form, not a chat — no composer renders', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const seed = await seedPmProject(request, authHeader, 'PM E2E NotChat')

  await loadAt(page, token, '/experts')
  await pmRowFor(page, seed.projectName).click()
  await expect(page.getByTestId('pm-expert-view')).toBeVisible()

  // The chat composer (ChatView/InputBar chrome) must never mount here.
  await expect(page.locator('.chat-container')).toHaveCount(0)
  await expect(page.locator('.input-bar')).toHaveCount(0)
  await expect(page.locator('.input-textarea')).toHaveCount(0)
  await expect(page.locator('.send-btn')).toHaveCount(0)
})

test('a project with no pending questions shows no badge and the empty-state copy', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const seed = await seedPmProject(request, authHeader, 'PM E2E Empty')

  await loadAt(page, token, '/experts')

  // No waiting indicator on this project's PM row, and (since every other
  // test resolved its questions) no global rail badge either.
  const row = pmRowFor(page, seed.projectName)
  await expect(row).toBeVisible()
  await expect(row.getByTestId('pm-expert-waiting-indicator')).toHaveCount(0)
  await expect(page.getByTestId('pm-expert-waiting-badge')).toHaveCount(0)

  // The Q&A view shows both empty states.
  await row.click()
  await expect(page.getByTestId('pm-expert-view')).toBeVisible()
  await expect(page.getByTestId('pm-pending-empty')).toBeVisible()
  await expect(page.getByTestId('pm-pending-empty')).toContainText(
    'No questions waiting for an answer.',
  )
  await expect(page.getByTestId('pm-decisions-empty')).toBeVisible()
  await expect(page.getByTestId('pm-decisions-empty')).toContainText('No decisions recorded yet.')
})
