import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Regression coverage for "todos linger after the card is done": once a
 * card lands on a terminal step, both the in-board summary on the kanban
 * and the dedicated /projects/:id/todos view must drop the now-stale
 * snapshot. The chat session view's TodoPanel must hide too — the agent
 * has finished, the scratchpad isn't live work anymore.
 *
 * The flow:
 *   1. mock:todo emits the canonical 3-item snapshot for the spawned worker.
 *   2. Pause so the orchestrator can't re-dispatch and clobber it.
 *   3. UI shows the in-board summary panel and the dedicated view both
 *      reflect that snapshot.
 *   4. Move the card to `done` via the same PUT the kanban DnD issues.
 *   5. The in-board summary disappears and the dedicated view shows the
 *      explicit empty state. The session-level /todos endpoint also
 *      returns an empty list (the standalone session-todos page reads
 *      from there).
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const MODEL = 'mock:todo'

type AuthBundle = {
  token: string
  authHeader: { Authorization: string }
}

async function authenticate(request: APIRequestContext): Promise<AuthBundle> {
  const status = await request.get('/api/auth/status')
  expect(status.ok()).toBeTruthy()
  const { has_users } = (await status.json()) as { has_users: boolean }

  const credentials = { username: E2E_USER, password: E2E_PASS }
  const endpoint = has_users ? '/api/auth/login' : '/api/auth/register'

  const res = await request.post(endpoint, { data: credentials })
  expect(res.ok(), `auth via ${endpoint} failed: ${await res.text()}`).toBeTruthy()
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

test('moving a card to done clears its todos from every view', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-todoclear-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-todoclear', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: authHeader,
    data: {
      name: 'todoclear project',
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
      title: 'Finish the parser',
      description: 'do work',
      step: 'backlog',
      priority: 0,
      model: MODEL,
    },
  })
  expect(cardRes.ok(), `create card failed: ${await cardRes.text()}`).toBeTruthy()
  const card = (await cardRes.json()) as Card

  await waitForCardSnapshot(request, authHeader, project.id, card.id, 30_000)

  const pauseRes = await request.post(`/api/projects/${project.id}/pause`, { headers: authHeader })
  expect(pauseRes.ok(), `pause failed: ${await pauseRes.text()}`).toBeTruthy()
  // After pause the card may briefly still point at an in-flight worker
  // before the completion listener clears it; poll for the session id
  // the card holds when the dust settles — that's the one bound to
  // every UI surface that displays this card's todos.
  const stableSid = await waitForCardSnapshot(request, authHeader, project.id, card.id, 10_000)

  await loadAppAt(page, token, `/projects/${project.id}`)

  // Pre-move: the in-board summary is visible.
  await expect(page.getByTestId('project-todo-summary')).toBeVisible({ timeout: 15_000 })
  await expect(page.getByTestId('todo-panel-count')).toHaveText('2 active')

  // Move the card to `done` via the same PUT the kanban DnD issues.
  // This is the user-finishes-the-card path; the orchestrator's
  // `handle_worker_done` is the worker-finishes path and is covered by
  // tests/card_completion.rs.
  const moveRes = await request.put(`/api/projects/${project.id}/cards/${card.id}`, {
    headers: authHeader,
    data: { step: 'done' },
  })
  expect(moveRes.ok(), `move to done failed: ${await moveRes.text()}`).toBeTruthy()

  // The in-board summary disappears because the card no longer has any
  // active items. The component returns null when there are zero groups,
  // so we assert it's gone outright rather than empty.
  await expect(page.getByTestId('project-todo-summary')).toHaveCount(0, { timeout: 10_000 })

  // The dedicated /projects/:id/todos view also reflects the cleared
  // state — no card group, explicit empty state.
  await loadAppAt(page, token, `/projects/${project.id}/todos`)
  await expect(page.getByTestId('project-todos-view')).toBeVisible({ timeout: 15_000 })
  await expect(page.getByTestId('project-todos-empty')).toBeVisible()
  await expect(page.getByTestId('project-todos-card-group')).toHaveCount(0)

  // And the session-level /todos endpoint that backs both the chat
  // session TodoPanel and the standalone session-todos view returns
  // an empty list for the card-bound session — the load-time fetch
  // agrees with the live event. We deliberately do NOT assert on
  // earlier abandoned worker sessions (the orchestrator may dispatch
  // a fresh worker between create and pause); those are no longer
  // navigable from this card, so their lingering snapshot is
  // invisible to the surfaces the bug was filed against.
  expect(await sessionTodoCount(request, authHeader, stableSid)).toBe(0)
})
