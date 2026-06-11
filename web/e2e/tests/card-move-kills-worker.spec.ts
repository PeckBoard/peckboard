import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Card moves and project pause must hard-stop the assigned worker — a
 * worker that keeps running after its card has moved would either advance
 * the now-stale step (corrupting the kanban) or sit forever holding a
 * worker slot that the user has already redirected. We exercise both
 * cases against the mock provider's `ask` scenario, which blocks
 * indefinitely waiting for stdin and so reliably models "long-running
 * worker that has to be cancelled."
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

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

async function makeProject(request: APIRequestContext, auth: Record<string, string>, name: string) {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-card-move-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-card-move-${Date.now()}-${name}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  // `mock:ask` blocks on stdin forever, so the orchestrator-spawned
  // worker stays alive until something cancels it. Perfect for testing
  // the move-cancels-worker path.
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name,
      folder_id: folder.id,
      worker_count: 1,
      workflow: 'task',
      model: 'mock:ask',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }
  return { folder, project }
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

/**
 * Poll the card until it picks up a worker_session_id (the orchestrator
 * tick runs every 5s; we give it generous slack). Returns the session id
 * the orchestrator assigned.
 */
async function waitForWorker(
  request: APIRequestContext,
  auth: Record<string, string>,
  projectId: string,
  cardId: string,
  timeoutMs: number,
): Promise<string> {
  const deadline = Date.now() + timeoutMs
  let lastCard: Record<string, unknown> | null = null
  while (Date.now() < deadline) {
    const res = await request.get(`/api/projects/${projectId}/cards`, { headers: auth })
    expect(res.ok()).toBeTruthy()
    const cards = (await res.json()) as Array<Record<string, unknown>>
    const card = cards.find((c) => c.id === cardId)
    if (card && typeof card.worker_session_id === 'string') {
      return card.worker_session_id
    }
    lastCard = card ?? null
    await new Promise((r) => setTimeout(r, 500))
  }
  throw new Error(
    `worker never spawned within ${timeoutMs}ms; last card state: ${JSON.stringify(lastCard)}`,
  )
}

async function waitFor(predicate: () => Promise<boolean>, timeoutMs: number, label: string) {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    if (await predicate()) return
    await new Promise((r) => setTimeout(r, 250))
  }
  throw new Error(`timed out waiting for: ${label}`)
}

/**
 * The `/api/sessions/:id/status` endpoint returns the derived status
 * string ("working", "tool-active", "questioning", "crashed", "idle",
 * …). Treat anything other than "idle" / "crashed" / "complete" as
 * "the worker is still alive doing something."
 */
async function fetchSessionStatus(
  request: APIRequestContext,
  auth: Record<string, string>,
  sessionId: string,
): Promise<string> {
  const res = await request.get(`/api/sessions/${sessionId}/status`, { headers: auth })
  expect(res.ok()).toBeTruthy()
  const body = (await res.json()) as { status?: string }
  return body.status ?? 'unknown'
}

async function isWorkerActive(
  request: APIRequestContext,
  auth: Record<string, string>,
  sessionId: string,
): Promise<boolean> {
  const status = await fetchSessionStatus(request, auth, sessionId)
  return status === 'working' || status === 'tool-active' || status === 'questioning'
}

async function isWorkerStopped(
  request: APIRequestContext,
  auth: Record<string, string>,
  sessionId: string,
): Promise<boolean> {
  const status = await fetchSessionStatus(request, auth, sessionId)
  return status === 'idle' || status === 'crashed' || status === 'complete'
}

test('moving a card to a different step cancels the running worker', async ({
  request,
  baseURL,
}) => {
  expect(baseURL).toBeTruthy()
  const { auth } = await authenticate(request)
  const { project } = await makeProject(request, auth, 'card-move-kills-worker')
  const card = await makeCard(request, auth, project.id, 'Long-running')

  // Wait for the orchestrator to spawn a worker. `mock:ask` blocks, so
  // the session sits live until we cancel it.
  const workerSessionId = await waitForWorker(request, auth, project.id, card.id, 20_000)

  // Sanity: the session is reported as in-flight. The orchestrator
  // bumped the step from `backlog` to the workflow's second step.
  await waitFor(
    async () => isWorkerActive(request, auth, workerSessionId),
    5_000,
    'worker to enter active state',
  )

  // Move the card to `done`. The route handler must (a) clear the
  // worker_session_id atomically, (b) cancel the running worker.
  const moveRes = await request.put(`/api/projects/${project.id}/cards/${card.id}`, {
    headers: auth,
    data: { step: 'done' },
  })
  expect(moveRes.ok(), `move failed: ${await moveRes.text()}`).toBeTruthy()
  const moved = (await moveRes.json()) as Record<string, unknown>
  expect(moved.step).toBe('done')
  expect(moved.worker_session_id).toBeNull()

  // The worker process must stop running. Cancellation is async; allow
  // a few seconds for the synthetic Crashed event to land.
  await waitFor(
    () => isWorkerStopped(request, auth, workerSessionId),
    10_000,
    'worker to stop running after card move',
  )

  // The card must NOT re-acquire a worker — `done` is terminal, the
  // orchestrator filter must skip it.
  await new Promise((r) => setTimeout(r, 6_000)) // one full orchestrator tick + slack
  const finalCards = (await (
    await request.get(`/api/projects/${project.id}/cards`, { headers: auth })
  ).json()) as Array<Record<string, unknown>>
  const finalCard = finalCards.find((c) => c.id === card.id)
  expect(finalCard?.worker_session_id ?? null).toBeNull()
  expect(finalCard?.step).toBe('done')
})

test('pausing a project cancels all in-flight workers and drops their queues', async ({
  request,
  baseURL,
}) => {
  expect(baseURL).toBeTruthy()
  const { auth } = await authenticate(request)
  const { project } = await makeProject(request, auth, 'pause-cancels-workers')
  // Two cards on a single-worker project: only one spawns at a time,
  // but the assigned one is what we want to verify the pause kills.
  const card = await makeCard(request, auth, project.id, 'Slow task')
  const workerSessionId = await waitForWorker(request, auth, project.id, card.id, 20_000)

  // Pre-condition: worker is running, project active.
  await waitFor(
    () => isWorkerActive(request, auth, workerSessionId),
    5_000,
    'worker to enter active state before pause',
  )

  const pauseRes = await request.post(`/api/projects/${project.id}/pause`, { headers: auth })
  expect(pauseRes.ok(), `pause failed: ${await pauseRes.text()}`).toBeTruthy()
  expect((await pauseRes.json()).status).toBe('paused')

  // The worker must stop. The pause path issues a cancel; with the
  // queued-message clear and the drain gate in place there is no path
  // for the listener to resurrect it.
  await waitFor(
    () => isWorkerStopped(request, auth, workerSessionId),
    10_000,
    'worker to stop running after pause',
  )

  // Even after a full orchestrator tick the paused project must NOT
  // get a fresh worker — the `status != "active"` filter is what holds
  // the line and we don't want a regression there to slip past.
  await new Promise((r) => setTimeout(r, 6_000))
  const cards = (await (
    await request.get(`/api/projects/${project.id}/cards`, { headers: auth })
  ).json()) as Array<Record<string, unknown>>
  for (const c of cards) {
    expect(
      c.worker_session_id ?? null,
      `card ${c.id} should have no worker while paused`,
    ).toBeNull()
  }

  // Resume puts the project back to active. The orchestrator's next
  // tick (or two) re-spawns a worker for the still-unassigned card —
  // proving "pause/resume" round-trips cleanly without leaving any
  // card permanently stranded.
  const resumeRes = await request.post(`/api/projects/${project.id}/resume`, { headers: auth })
  expect(resumeRes.ok(), `resume failed: ${await resumeRes.text()}`).toBeTruthy()

  await waitFor(
    async () => {
      const res = await request.get(`/api/projects/${project.id}/cards`, { headers: auth })
      const cs = (await res.json()) as Array<Record<string, unknown>>
      return cs.some((c) => typeof c.worker_session_id === 'string')
    },
    20_000,
    'orchestrator to re-spawn a worker after resume',
  )
})
