import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e test for the project-page todo/task aggregate view.
 *
 * Drives the real React app end-to-end: creates a project + card whose worker
 * is dispatched by the orchestrator with the deterministic `mock:todo`
 * scenario. That scenario emits a `todo` event whose snapshot is
 *   done: "Write the parser", in_progress: "Wire up the route", pending: "Add tests".
 *
 * Once the snapshot exists in the worker session's event log, the project is
 * paused (so the orchestrator can't re-dispatch and churn the snapshot), the
 * board is loaded, and we assert:
 *   - the kanban card shows a "1/3" progress badge,
 *   - the aggregate panel groups the work items by Pending / In Progress / Done
 *     with the right counts.
 * Then a fresh `todo` snapshot is POSTed over the same WS broadcast path the
 * real provider uses, and we assert the aggregate re-buckets live (latest wins).
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const MODEL = 'mock:todo'

type AuthBundle = {
  token: string
  authHeader: { Authorization: string }
}

async function authenticate(request: APIRequestContext): Promise<AuthBundle> {
  // The server auto-bootstraps the admin from PECKBOARD_BOOTSTRAP_*
  // env vars at first start (see playwright.config.ts); we just log in.
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

type Card = {
  id: string
  worker_session_id: string | null
  last_worker_session_id: string | null
}

async function getCard(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  projectId: string,
  cardId: string,
): Promise<Card> {
  const res = await request.get(`/api/projects/${projectId}/cards`, { headers: authHeader })
  expect(res.ok(), `list cards failed: ${await res.text()}`).toBeTruthy()
  const cards = (await res.json()) as Card[]
  const card = cards.find((c) => c.id === cardId)
  expect(card, `card ${cardId} present`).toBeTruthy()
  return card!
}

async function sessionTodoCount(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  sessionId: string,
): Promise<number> {
  const res = await request.get(`/api/sessions/${sessionId}/todos`, { headers: authHeader })
  if (!res.ok()) return 0
  const { todos } = (await res.json()) as { todos: unknown[] }
  return Array.isArray(todos) ? todos.length : 0
}

/**
 * Poll until the card's worker session has reported the full 3-item snapshot,
 * returning that session id. The orchestrator runs on a tick, so the worker is
 * dispatched (and emits) a short while after the card is created.
 */
async function waitForCardSnapshot(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  projectId: string,
  cardId: string,
  timeoutMs: number,
): Promise<string> {
  const deadline = Date.now() + timeoutMs
  let lastSid: string | null = null
  while (Date.now() < deadline) {
    const card = await getCard(request, authHeader, projectId, cardId)
    const sid = card.worker_session_id ?? card.last_worker_session_id
    if (sid) {
      lastSid = sid
      if ((await sessionTodoCount(request, authHeader, sid)) === 3) return sid
    }
    await new Promise((r) => setTimeout(r, 750))
  }
  throw new Error(`card ${cardId} never produced a 3-item todo snapshot (last session: ${lastSid})`)
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

test('project page aggregates worker todos and updates live', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-agg-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-agg', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: authHeader,
    data: {
      name: 'agg project',
      folder_id: folder.id,
      worker_count: 1,
      model: MODEL,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: authHeader,
    data: {
      title: 'Ship the parser',
      description: 'do work',
      step: 'backlog',
      priority: 0,
      model: MODEL,
    },
  })
  expect(cardRes.ok(), `create card failed: ${await cardRes.text()}`).toBeTruthy()
  const card = (await cardRes.json()) as Card

  // Wait for the orchestrator to dispatch the worker and the mock snapshot to
  // land in its event log.
  await waitForCardSnapshot(request, authHeader, project.id, card.id, 30_000)

  // Pause so the orchestrator can't re-dispatch and overwrite the snapshot we
  // are about to assert against / update live.
  const pauseRes = await request.post(`/api/projects/${project.id}/pause`, { headers: authHeader })
  expect(pauseRes.ok(), `pause failed: ${await pauseRes.text()}`).toBeTruthy()

  // Re-read the now-stable session id (a final tick may have re-spawned before
  // the pause took effect, which creates a fresh session).
  const sessionId = await waitForCardSnapshot(request, authHeader, project.id, card.id, 10_000)

  await loadAppAt(page, token, `/projects/${project.id}`)

  // The aggregate panel renders the rolled-up snapshot.
  const panel = page.getByTestId('project-todo-summary')
  await expect(panel).toBeVisible({ timeout: 15_000 })
  await expect(page.getByTestId('todo-panel-count')).toHaveText('1/3 done')

  // Per-card progress badge on the kanban card.
  await expect(page.getByTestId('card-todo-badge')).toHaveText('1/3')

  // Items grouped by status with the right counts.
  await expect(page.locator('[data-testid="todo-item"][data-status="done"]')).toHaveCount(1)
  await expect(page.locator('[data-testid="todo-item"][data-status="done"]')).toContainText(
    'Write the parser',
  )
  await expect(page.locator('[data-testid="todo-item"][data-status="in_progress"]')).toContainText(
    'Wiring up the route',
  )
  await expect(page.locator('[data-testid="todo-item"][data-status="pending"]')).toContainText(
    'Add tests',
  )

  // Live update: emit a fresh snapshot over the same WS broadcast path the real
  // provider uses (latest `todo` event wins). Two done, one in progress, none
  // pending.
  const updateRes = await request.post(`/api/sessions/${sessionId}/events`, {
    headers: authHeader,
    data: {
      kind: 'todo',
      data: {
        todos: [
          { content: 'Write the parser', status: 'done', activeForm: 'Writing the parser' },
          { content: 'Wire up the route', status: 'done', activeForm: 'Wiring up the route' },
          { content: 'Add tests', status: 'in_progress', activeForm: 'Adding tests' },
        ],
      },
    },
  })
  expect(updateRes.ok(), `update event failed: ${await updateRes.text()}`).toBeTruthy()

  // The aggregate re-buckets live: 2/3 done, pending group gone, in-progress now
  // shows "Adding tests"; the card badge tracks it too.
  await expect(page.getByTestId('todo-panel-count')).toHaveText('2/3 done')
  await expect(page.getByTestId('card-todo-badge')).toHaveText('2/3')
  await expect(page.locator('[data-testid="todo-item"][data-status="done"]')).toHaveCount(2)
  await expect(page.locator('[data-testid="todo-item"][data-status="pending"]')).toHaveCount(0)
  await expect(page.locator('[data-testid="todo-item"][data-status="in_progress"]')).toContainText(
    'Adding tests',
  )
})
