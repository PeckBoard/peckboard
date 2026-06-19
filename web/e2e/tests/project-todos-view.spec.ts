import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e test for the dedicated project-todos view + its launch button on the
 * Kanban header. The in-board `ProjectTodoSummary` is already covered by
 * `project-todo-aggregate.spec.ts`; this spec exercises the always-reachable
 * `/projects/{id}/todos` route specifically:
 *   - the Todos button on the Kanban header navigates there,
 *   - the view renders the aggregate even when the in-board summary would too,
 *   - a project with no card-todos shows the explicit empty state,
 *   - the Back affordance returns to the board.
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

async function createFolder(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  slug: string,
): Promise<{ id: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-${slug}-`))
  const res = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-${slug}`, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as { id: string }
}

test('Todos button opens dedicated view that aggregates worker todos', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const folder = await createFolder(request, authHeader, 'todosview')

  const projectRes = await request.post('/api/projects', {
    headers: authHeader,
    data: {
      name: 'todos view project',
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

  await waitForCardSnapshot(request, authHeader, project.id, card.id, 30_000)

  // Pause to prevent the orchestrator from re-dispatching and clobbering the
  // snapshot while we drive the UI.
  const pauseRes = await request.post(`/api/projects/${project.id}/pause`, { headers: authHeader })
  expect(pauseRes.ok(), `pause failed: ${await pauseRes.text()}`).toBeTruthy()
  await waitForCardSnapshot(request, authHeader, project.id, card.id, 10_000)

  await loadAppAt(page, token, `/projects/${project.id}`)

  // The Todos button is on the Kanban header and navigates to the dedicated view.
  const todosBtn = page.getByTestId('project-todos-button')
  await expect(todosBtn).toBeVisible({ timeout: 15_000 })
  await todosBtn.click()

  await expect(page).toHaveURL(new RegExp(`/projects/${project.id}/todos$`))
  const view = page.getByTestId('project-todos-view')
  await expect(view).toBeVisible()

  // Card group renders with the card title and a TodoPanel.
  const group = page.getByTestId('project-todos-card-group')
  await expect(group).toHaveCount(1)
  await expect(group).toContainText('Ship the parser')
  await expect(view.getByTestId('todo-panel-count')).toHaveText('1/3 done')
  await expect(view.locator('[data-testid="todo-item"][data-status="done"]')).toContainText(
    'Write the parser',
  )
  await expect(view.locator('[data-testid="todo-item"][data-status="in_progress"]')).toContainText(
    'Wiring up the route',
  )
  await expect(view.locator('[data-testid="todo-item"][data-status="pending"]')).toContainText(
    'Add tests',
  )

  // The empty state must NOT show when there are todos.
  await expect(page.getByTestId('project-todos-empty')).toHaveCount(0)

  // Back button returns to the board view.
  await page.locator('.project-todos-header button', { hasText: 'Back' }).click()
  await expect(page).toHaveURL(new RegExp(`/projects/${project.id}$`))
  await expect(page.getByTestId('project-todos-button')).toBeVisible()
})

test('dedicated project todos view renders explicit empty state when there are no todos', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const folder = await createFolder(request, authHeader, 'todosempty')

  // Paused project with no cards — no worker sessions, no todos.
  const projectRes = await request.post('/api/projects', {
    headers: authHeader,
    data: {
      name: 'empty todos project',
      folder_id: folder.id,
      worker_count: 1,
      model: MODEL,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }
  await request.post(`/api/projects/${project.id}/pause`, { headers: authHeader })

  await loadAppAt(page, token, `/projects/${project.id}/todos`)

  await expect(page.getByTestId('project-todos-view')).toBeVisible({ timeout: 15_000 })
  await expect(page.getByTestId('project-todos-empty')).toBeVisible()
  await expect(page.getByTestId('project-todos-card-group')).toHaveCount(0)
})
