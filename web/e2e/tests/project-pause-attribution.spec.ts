import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync, readFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

/**
 * Every pause and resume of a project records who made it, and the board
 * says so:
 *
 *  - a person pausing shows "paused by <username>", and can resume from the
 *    banner;
 *  - an agent session pausing through its MCP tool shows
 *    `agent session "<name>"`, and its resume reaches the open board;
 *  - a status written through `PUT /api/projects/{id}` or the
 *    `update_project` tool is a real, recorded pause, and an unknown status
 *    is refused.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const AGENT_NAME = 'Driver bot'

// Screenshots for a visual check of the banner (light, dark, narrow).
// Written next to the repo; not asserted on.
const SHOTS = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  '../../../.tmp-pause-shots',
)

type StatusEntry = {
  id: string
  action: string
  actor_kind: string
  actor_id: string | null
  actor_name: string
  at: string
}
type ProjectJson = {
  id: string
  status: string
  pause_reason: string | null
  last_status_change: StatusEntry | null
}

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

/** A folder with one project no worker ever touches. */
async function seedProject(request: APIRequestContext, auth: Record<string, string>, tag: string) {
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: {
      name: `e2e-pause-${tag}-${Date.now()}`,
      path: mkdtempSync(path.join(tmpdir(), `peckboard-e2e-pause-${tag}-`)),
    },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folderId = ((await folderRes.json()) as { id: string }).id
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: { name: `pause ${tag}`, folder_id: folderId, worker_count: 0, workflow: 'task' },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const projectId = ((await projectRes.json()) as { id: string }).id
  return { folderId, projectId }
}

/** A chat session in the project's folder with a live `mock:block` agent, so
 *  its per-session MCP token stays valid for the tool calls below. */
async function seedAgent(
  request: APIRequestContext,
  auth: Record<string, string>,
  folderId: string,
) {
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: AGENT_NAME, folder_id: folderId },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
    headers: auth,
    data: { text: 'go', model: 'mock:block' },
  })
  expect(sendRes.ok(), `send message failed: ${await sendRes.text()}`).toBeTruthy()
  const cfgPath = path.join(process.env.PECKBOARD_E2E_DATA_DIR!, 'worker-mcp', `${session.id}.json`)
  await expect
    .poll(() => {
      try {
        readFileSync(cfgPath, 'utf8')
        return true
      } catch {
        return false
      }
    })
    .toBe(true)
  const cfg = JSON.parse(readFileSync(cfgPath, 'utf8')) as {
    mcpServers: { peckboard: { headers: { Authorization: string } } }
  }
  return {
    sessionId: session.id,
    mcpToken: cfg.mcpServers.peckboard.headers.Authorization.replace(/^Bearer /, ''),
  }
}

/** Calls one MCP tool with the session's own token; `failed` is true when
 *  the tool refused (a JSON-RPC error or an `isError` result). */
async function mcpCall(
  request: APIRequestContext,
  mcpToken: string,
  tool: string,
  args: Record<string, unknown>,
) {
  const res = await request.post('/mcp', {
    headers: { Authorization: `Bearer ${mcpToken}` },
    data: { jsonrpc: '2.0', id: 1, method: 'tools/call', params: { name: tool, arguments: args } },
  })
  expect(res.ok(), `${tool}: HTTP ${res.status()}`).toBeTruthy()
  const body = (await res.json()) as { error?: unknown; result?: { isError?: boolean } }
  return { failed: !!body.error || !!body.result?.isError, text: JSON.stringify(body) }
}

async function mcpOk(
  request: APIRequestContext,
  mcpToken: string,
  tool: string,
  args: Record<string, unknown>,
) {
  const out = await mcpCall(request, mcpToken, tool, args)
  expect(out.failed, `${tool} failed: ${out.text}`).toBe(false)
}

async function getProject(
  request: APIRequestContext,
  auth: Record<string, string>,
  projectId: string,
) {
  const res = await request.get(`/api/projects/${projectId}`, { headers: auth })
  expect(res.ok(), `get project failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { project: ProjectJson }).project
}

async function openBoard(page: Page, token: string, projectId: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(`/projects/${projectId}`)
  await expect(page.locator('.kanban-columns')).toBeVisible({ timeout: 15_000 })
}

test('a person pausing is named on the board and can resume from the banner', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const { projectId } = await seedProject(request, auth, 'person')

  const pauseRes = await request.post(`/api/projects/${projectId}/pause`, { headers: auth })
  expect(pauseRes.ok(), `pause failed: ${await pauseRes.text()}`).toBeTruthy()
  const paused = (await pauseRes.json()) as ProjectJson
  expect(paused.pause_reason).toBeNull()
  expect(paused.last_status_change).toMatchObject({
    action: 'paused',
    actor_kind: 'user',
    actor_name: E2E_USER,
  })

  await openBoard(page, token, projectId)
  const banner = page.getByTestId('project-paused-banner')
  await expect(banner).toContainText(`paused by ${E2E_USER}`)

  await banner.getByTestId('project-paused-resume').click()
  await expect(banner).toHaveCount(0)
  const resumed = await getProject(request, auth, projectId)
  expect(resumed.status).toBe('active')
  expect(resumed.last_status_change).toMatchObject({
    action: 'resumed',
    actor_kind: 'user',
    actor_name: E2E_USER,
  })
})

test('an agent pausing is named, its resume reaches the open board, and History lists both', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const { folderId, projectId } = await seedProject(request, auth, 'agent')
  const { sessionId, mcpToken } = await seedAgent(request, auth, folderId)

  await openBoard(page, token, projectId)
  const banner = page.getByTestId('project-paused-banner')
  await expect(banner).toHaveCount(0)

  await mcpOk(request, mcpToken, 'pause_project', { project_id: projectId })
  await expect(banner).toContainText(`paused by agent session "${AGENT_NAME}"`)
  const paused = await getProject(request, auth, projectId)
  expect(paused.status).toBe('paused')
  expect(paused.pause_reason).toContain(AGENT_NAME)
  expect(paused.last_status_change).toMatchObject({
    action: 'paused',
    actor_kind: 'session',
    actor_id: sessionId,
    actor_name: AGENT_NAME,
  })
  // The agent sentence is not repeated as a second line.
  await expect(banner.locator('.project-paused-banner-reason')).toHaveCount(0)

  // The regression: a resume through the tool used not to tell the board.
  await mcpOk(request, mcpToken, 'resume_project', { project_id: projectId })
  await expect(banner).toHaveCount(0)
  expect((await getProject(request, auth, projectId)).last_status_change).toMatchObject({
    action: 'resumed',
    actor_kind: 'session',
    actor_name: AGENT_NAME,
  })

  // Paused again, by the person this time, so the banner is back.
  const again = await request.post(`/api/projects/${projectId}/pause`, { headers: auth })
  expect(again.ok()).toBeTruthy()
  await expect(banner).toContainText(`paused by ${E2E_USER}`)
  await banner.getByTestId('project-status-history-toggle').click()
  const rows = banner
    .getByTestId('project-status-history')
    .getByTestId('project-status-history-row')
  await expect(rows).toHaveCount(3)
  await expect(rows.nth(0)).toContainText(`Paused by ${E2E_USER}`)
  await expect(rows.nth(1)).toContainText(`Resumed by agent session "${AGENT_NAME}"`)
  await expect(rows.nth(2)).toContainText(`Paused by agent session "${AGENT_NAME}"`)

  // A new entry lands in the open list without reopening it.
  await mcpOk(request, mcpToken, 'pause_project', { project_id: projectId })
  await expect(rows).toHaveCount(4)
  await expect(rows.nth(0)).toContainText(`Paused by agent session "${AGENT_NAME}"`)

  // The dashboard's project widget uses the same wording.
  const viewRes = await request.post('/api/me/views', {
    headers: auth,
    data: {
      name: `pause attribution ${Date.now()}`,
      widgets: [{ id: 'w-project', kind: 'project', x: 0, y: 0, w: 6, h: 10, projectId }],
    },
  })
  expect(viewRes.status(), await viewRes.text()).toBe(201)
  await page.goto(`/views/${((await viewRes.json()) as { id: string }).id}`)
  await expect(page.getByTestId('project-widget-pause-reason')).toHaveText(
    `Paused by agent session "${AGENT_NAME}"`,
    { timeout: 15_000 },
  )
})

test('a status written through update is a recorded pause, and an unknown one is refused', async ({
  request,
}) => {
  const { auth } = await authenticate(request)
  const { folderId, projectId } = await seedProject(request, auth, 'side-door')
  const { mcpToken } = await seedAgent(request, auth, folderId)

  const bogus = await request.put(`/api/projects/${projectId}`, {
    headers: auth,
    data: { status: 'bogus' },
  })
  expect(bogus.status()).toBe(400)
  expect((await getProject(request, auth, projectId)).status).toBe('active')

  const put = await request.put(`/api/projects/${projectId}`, {
    headers: auth,
    data: { status: 'paused' },
  })
  expect(put.status(), await put.text()).toBe(200)
  const byUser = await getProject(request, auth, projectId)
  expect(byUser.status).toBe('paused')
  expect(byUser.last_status_change).toMatchObject({
    action: 'paused',
    actor_kind: 'user',
    actor_name: E2E_USER,
  })

  const resume = await request.post(`/api/projects/${projectId}/resume`, { headers: auth })
  expect(resume.ok()).toBeTruthy()

  await mcpOk(request, mcpToken, 'update_project', { project_id: projectId, status: 'paused' })
  const byAgent = await getProject(request, auth, projectId)
  expect(byAgent.status).toBe('paused')
  expect(byAgent.last_status_change).toMatchObject({
    action: 'paused',
    actor_kind: 'session',
    actor_name: AGENT_NAME,
  })

  const refused = await mcpCall(request, mcpToken, 'update_project', {
    project_id: projectId,
    status: 'bogus',
  })
  expect(refused.failed, `bogus status was accepted: ${refused.text}`).toBe(true)
  expect((await getProject(request, auth, projectId)).status).toBe('paused')
})

test('screenshots of the paused banner with its history', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const { folderId, projectId } = await seedProject(request, auth, 'shots')
  const { mcpToken } = await seedAgent(request, auth, folderId)
  expect((await request.post(`/api/projects/${projectId}/pause`, { headers: auth })).ok()).toBe(
    true,
  )
  expect((await request.post(`/api/projects/${projectId}/resume`, { headers: auth })).ok()).toBe(
    true,
  )
  await mcpOk(request, mcpToken, 'pause_project', { project_id: projectId })

  await openBoard(page, token, projectId)
  const banner = page.getByTestId('project-paused-banner')
  await banner.getByTestId('project-status-history-toggle').click()
  await expect(banner.getByTestId('project-status-history-row')).toHaveCount(3)

  for (const scheme of ['light', 'dark'] as const) {
    await page.emulateMedia({ colorScheme: scheme })
    // Let the theme transition finish.
    await page.waitForTimeout(600)
    await page.screenshot({ path: path.join(SHOTS, `banner-${scheme}.png`) })
    await page.setViewportSize({ width: 380, height: 800 })
    await page.screenshot({ path: path.join(SHOTS, `banner-${scheme}-380.png`) })
    await page.setViewportSize({ width: 1280, height: 720 })
  }
  await page.emulateMedia({ colorScheme: null })
})
