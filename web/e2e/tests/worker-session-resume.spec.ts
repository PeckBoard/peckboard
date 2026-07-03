import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Worker-session resume: a card that leaves `in_progress` only via a
 * detour (backlog, wont_do, or the blocked flag) must get its PREVIOUS
 * worker session back when it is picked up again — same session id, same
 * conversation — instead of a freshly minted session that has lost all
 * context. Moving the card forward to a different real step severs that
 * link and the next step gets a fresh session.
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

async function makeProject(
  request: APIRequestContext,
  auth: Record<string, string>,
  name: string,
  workflow: string,
) {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-resume-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-resume-${Date.now()}-${name}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  // `mock:ask` blocks on stdin forever, so the orchestrator-spawned
  // worker stays alive until a card move cancels it — modelling a
  // long-running worker that gets interrupted mid-task.
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name,
      folder_id: folder.id,
      worker_count: 1,
      workflow,
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

async function getCard(
  request: APIRequestContext,
  auth: Record<string, string>,
  projectId: string,
  cardId: string,
): Promise<Record<string, unknown>> {
  const res = await request.get(`/api/projects/${projectId}/cards`, { headers: auth })
  expect(res.ok()).toBeTruthy()
  const cards = (await res.json()) as Array<Record<string, unknown>>
  const card = cards.find((c) => c.id === cardId)
  expect(card, `card ${cardId} disappeared`).toBeTruthy()
  return card as Record<string, unknown>
}

/** Poll until the card has a worker_session_id, return it. */
async function waitForWorker(
  request: APIRequestContext,
  auth: Record<string, string>,
  projectId: string,
  cardId: string,
  timeoutMs: number,
): Promise<string> {
  const deadline = Date.now() + timeoutMs
  let last: Record<string, unknown> | null = null
  while (Date.now() < deadline) {
    const card = await getCard(request, auth, projectId, cardId)
    if (typeof card.worker_session_id === 'string') {
      return card.worker_session_id
    }
    last = card
    await new Promise((r) => setTimeout(r, 500))
  }
  throw new Error(
    `worker never spawned within ${timeoutMs}ms; last card state: ${JSON.stringify(last)}`,
  )
}

async function moveCard(
  request: APIRequestContext,
  auth: Record<string, string>,
  projectId: string,
  cardId: string,
  step: string,
) {
  const res = await request.put(`/api/projects/${projectId}/cards/${cardId}`, {
    headers: auth,
    data: { step },
  })
  expect(res.ok(), `move to ${step} failed: ${await res.text()}`).toBeTruthy()
}
// Each pickup can take several orchestrator ticks (5s each): the first
// spawn, then the cancel landing, then a tick possibly skipped while the
// cancelled agent winds down. The default 30s per-test budget is too
// tight on a loaded machine, so give these tests explicit headroom.
test.describe.configure({ timeout: 120_000 })

test('card moved to backlog and picked up again resumes the same worker session', async ({
  request,
  baseURL,
}) => {
  expect(baseURL).toBeTruthy()
  const { auth } = await authenticate(request)
  const { project } = await makeProject(request, auth, 'resume-same-session', 'task')
  const card = await makeCard(request, auth, project.id, 'Interruptible work')

  // First pickup: orchestrator advances backlog → in_progress and spawns.
  const firstSession = await waitForWorker(request, auth, project.id, card.id, 20_000)

  // User drags the card back to backlog — worker is cancelled, but the
  // resume link must survive this detour.
  await moveCard(request, auth, project.id, card.id, 'backlog')

  // The orchestrator re-picks the card on a later tick (the cancel has to
  // land first, so allow a couple of ticks). It must RESUME the previous
  // session rather than mint a new one.
  const secondSession = await waitForWorker(request, auth, project.id, card.id, 30_000)
  expect(secondSession).toBe(firstSession)
})

test('card advanced to the next step gets a fresh worker session', async ({ request, baseURL }) => {
  expect(baseURL).toBeTruthy()
  const { auth } = await authenticate(request)
  // deep-develop-software: backlog → in_progress → review → done, so an
  // in-flight card can be moved forward to a real (non-terminal) step.
  const { project } = await makeProject(
    request,
    auth,
    'fresh-after-advance',
    'deep-develop-software',
  )
  const card = await makeCard(request, auth, project.id, 'Two-step work')

  const firstSession = await waitForWorker(request, auth, project.id, card.id, 20_000)

  // User drags the card forward to `review`. That severs the previous
  // session's resume link; the review pickup must be a NEW session.
  await moveCard(request, auth, project.id, card.id, 'review')

  const deadline = Date.now() + 30_000
  let reviewSession: string | null = null
  while (Date.now() < deadline) {
    const c = await getCard(request, auth, project.id, card.id)
    if (typeof c.worker_session_id === 'string' && c.worker_session_id !== firstSession) {
      reviewSession = c.worker_session_id
      break
    }
    await new Promise((r) => setTimeout(r, 500))
  }
  expect(reviewSession, 'review step never got its own worker').toBeTruthy()
  expect(reviewSession).not.toBe(firstSession)
})
