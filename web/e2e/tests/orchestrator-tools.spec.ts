import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Orchestrator management MCP tools (session-control ≥ 0.5.0), end to end.
 *
 * A `mock:mcp` session runs real `orchestrator_*` tool calls (```mcp blocks
 * in its prompt). Create asks the user first: the approval question shows
 * in the chat, the user picks "Approve once", and re-calling the same tool
 * creates the orchestrator — which then shows on the Orchestrators page. A
 * rename (a narrowing update) applies without asking and the open page
 * picks it up live; delete asks again and the card disappears.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type Auth = { token: string; auth: Record<string, string> }

async function authenticate(request: APIRequestContext): Promise<Auth> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

function mcpBlock(tool: string, args: Record<string, unknown>): string {
  return '```mcp\n' + JSON.stringify({ tool, args }) + '\n```'
}

type Orch = { id: string; name: string; enabled: boolean; model: string | null }
type LoggedEvent = { id: string; kind: string; data: Record<string, unknown> }

async function listOrchestrators(request: APIRequestContext, auth: Record<string, string>) {
  const res = await request.get('/api/plugin-ui/session-control/orchestrators', { headers: auth })
  expect(res.ok(), `list orchestrators failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { orchestrators: Orch[] }).orchestrators
}

async function send(
  request: APIRequestContext,
  auth: Record<string, string>,
  sessionId: string,
  text: string,
) {
  const res = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth,
    data: { text, model: 'mock:mcp' },
  })
  expect(res.ok(), `send message failed: ${await res.text()}`).toBeTruthy()
}

/** The id of the newest question whose text contains `needle`. */
async function waitForQuestion(
  request: APIRequestContext,
  auth: Record<string, string>,
  sessionId: string,
  needle: string,
): Promise<string> {
  let id = ''
  await expect
    .poll(
      async () => {
        const res = await request.get(`/api/sessions/${sessionId}/events?after_seq=0`, {
          headers: auth,
        })
        const events = (await res.json()) as LoggedEvent[]
        const q = events
          .filter((e) => e.kind === 'question' && JSON.stringify(e.data).includes(needle))
          .pop()
        id = q?.id ?? ''
        return id
      },
      { timeout: 20_000 },
    )
    .not.toBe('')
  return id
}

test('agent creates, updates, and deletes an orchestrator via MCP tools', async ({
  request,
  page,
  baseURL,
}) => {
  test.setTimeout(120_000)
  expect(baseURL).toBeTruthy()
  const { token, auth } = await authenticate(request)

  const catalog = await (await request.get('/api/plugins', { headers: auth })).json()
  test.skip(
    !JSON.stringify(catalog).includes('session-control'),
    'session-control plugin not loaded',
  )
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-orch-tools-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-orch-tools-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'orchestrator tools', folder_id: folder.id, model: 'mock:mcp' },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  // Disabled, so the engine never fires a brain on the shared e2e server.
  const name = `Tool-made ${Date.now()}`
  const createMsg = `make one\n${mcpBlock('orchestrator_create', {
    name,
    goal: 'Prove the orchestrator tools work',
    model: 'mock:mcp',
    enabled: false,
  })}`

  // 1. Create asks first; the user approves in the chat's question modal.
  await loadAt(page, token, `/sessions/${session.id}`)
  await send(request, auth, session.id, createMsg)
  const modal = page.getByTestId('question-modal')
  await expect(modal).toBeVisible({ timeout: 20_000 })
  await expect(modal).toContainText(`Create orchestrator "${name}"`)
  expect((await listOrchestrators(request, auth)).some((o) => o.name === name)).toBe(false)
  await modal.getByText('Approve once').click()
  await modal.getByRole('button', { name: 'Submit' }).click()
  await expect(modal).toHaveCount(0, { timeout: 10_000 })

  // 2. Re-calling with the same arguments creates it.
  await send(request, auth, session.id, createMsg)
  let created: Orch | undefined
  await expect
    .poll(
      async () => {
        created = (await listOrchestrators(request, auth)).find((o) => o.name === name)
        return !!created
      },
      { timeout: 20_000 },
    )
    .toBe(true)
  expect(created!.enabled).toBe(false)
  expect(created!.model).toBe('mock:mcp')

  // 3. It shows on the Orchestrators page.
  await page.getByTestId('plugin-sidebar-session-control-orchestrators').click()
  const frame = page.frameLocator('[data-testid="plugin-fullpage-frame"]')
  await expect(frame.locator('[data-testid="orch-card"]', { hasText: name })).toBeVisible({
    timeout: 20_000,
  })

  // 4. A rename applies without asking; the open page refreshes live.
  const renamed = `${name} renamed`
  await send(
    request,
    auth,
    session.id,
    `rename\n${mcpBlock('orchestrator_update', { id: created!.id, name: renamed })}`,
  )
  await expect(frame.locator('[data-testid="orch-card"]', { hasText: renamed })).toBeVisible({
    timeout: 20_000,
  })

  // 5. Delete asks again (answered here through the same endpoint the
  //    question modal posts to), then the re-call removes the card.
  const deleteMsg = `remove\n${mcpBlock('orchestrator_delete', { id: created!.id })}`
  await send(request, auth, session.id, deleteMsg)
  const questionId = await waitForQuestion(request, auth, session.id, 'Delete orchestrator')
  expect((await listOrchestrators(request, auth)).some((o) => o.id === created!.id)).toBe(true)
  const answer = await request.post(`/api/sessions/${session.id}/events`, {
    headers: auth,
    data: {
      kind: 'question-resolved',
      data: { question_id: questionId, answers: { '0': 'Approve once' } },
    },
  })
  expect(answer.ok(), `answer failed: ${await answer.text()}`).toBeTruthy()
  await send(request, auth, session.id, deleteMsg)
  await expect
    .poll(async () => (await listOrchestrators(request, auth)).some((o) => o.id === created!.id), {
      timeout: 20_000,
    })
    .toBe(false)
  await expect(frame.locator('[data-testid="orch-card"]', { hasText: renamed })).toHaveCount(0, {
    timeout: 20_000,
  })

  await request.delete(`/api/sessions/${session.id}`, { headers: auth })
})

test("deleting an orchestrator from the page stops its brain's agent but keeps the session", async ({
  request,
  page,
}) => {
  test.setTimeout(120_000)
  const { token, auth } = await authenticate(request)
  const catalog = await (await request.get('/api/plugins', { headers: auth })).json()
  test.skip(
    !JSON.stringify(catalog).includes('session-control'),
    'session-control plugin not loaded',
  )
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-orch-del-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-orch-del-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  // Disabled (no scheduled fires); `mock:slow` keeps the brain's turn
  // running for 30s once "Run now" fires it.
  const name = `Brain stop ${Date.now()}`
  const createdRes = await request.post('/api/plugin-ui/session-control/orchestrators', {
    headers: auth,
    data: {
      name,
      folder_id: folder.id,
      goal: 'Prove delete stops the brain',
      model: 'mock:slow',
      prompt: 'sleep:30 {{goal}}',
      enabled: false,
    },
  })
  expect(createdRes.ok(), `create failed: ${await createdRes.text()}`).toBeTruthy()
  const id = ((await createdRes.json()) as { id: string }).id

  // Run now (retrying until the engine clock has ticked once).
  await expect
    .poll(
      async () =>
        (
          await request.post(`/api/plugin-ui/session-control/orchestrators/${id}/run`, {
            headers: auth,
          })
        ).ok(),
      { timeout: 30_000 },
    )
    .toBe(true)
  let brain = ''
  await expect
    .poll(
      async () => {
        const o = (await listOrchestrators(request, auth)).find((x) => x.id === id) as
          | (Orch & { session_id?: string | null })
          | undefined
        brain = o?.session_id ?? ''
        return brain
      },
      { timeout: 20_000 },
    )
    .not.toBe('')
  const brainEvents = async () =>
    JSON.stringify(
      await (
        await request.get(`/api/sessions/${brain}/events?after_seq=0`, { headers: auth })
      ).json(),
    )
  await expect.poll(brainEvents, { timeout: 20_000 }).toContain('slow child working')

  // Delete from the page: an inline confirm (the sandboxed iframe can't
  // open window.confirm) says the brain's agent is stopped; Cancel backs out.
  await loadAt(page, token, '/')
  await page.getByTestId('plugin-sidebar-session-control-orchestrators').click()
  const frame = page.frameLocator('[data-testid="plugin-fullpage-frame"]')
  const card = frame.locator('[data-testid="orch-card"]', { hasText: name })
  await expect(card).toBeVisible({ timeout: 20_000 })
  await card.getByTestId('orch-delete').click()
  await expect(card.getByTestId('orch-delete-prompt')).toContainText("brain's agent is stopped")
  await card.getByRole('button', { name: 'Cancel' }).click()
  await expect(card.getByTestId('orch-delete-prompt')).toHaveCount(0)
  await card.getByTestId('orch-delete').click()
  await card.getByTestId('orch-delete-confirm').click()
  await expect(card).toHaveCount(0, { timeout: 20_000 })

  // The brain's run is terminated mid-turn; the session and transcript stay.
  await expect
    .poll(brainEvents, { timeout: 20_000 })
    .toContain('Agent terminated by session-control')
  expect(await brainEvents()).not.toContain('slow child done')
  const brainRes = await request.get(`/api/sessions/${brain}`, { headers: auth })
  expect(brainRes.ok(), 'brain session is kept').toBeTruthy()

  await request.delete(`/api/sessions/${brain}`, { headers: auth })
})
