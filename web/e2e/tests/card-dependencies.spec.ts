import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * End-to-end test for card dependencies.
 *
 * A card may declare that it depends on other cards. The orchestrator
 * must NOT hand a worker to a dependent card until every dependency has
 * reached `done` (a `wont_do` prerequisite does NOT satisfy it). This
 * spec drives the orchestrator with the deterministic mock provider:
 *
 *   - Create cards A and B where B depends on A, with two worker slots so
 *     B is gated by the dependency rather than by slot contention.
 *   - Assert A gets picked up while B stays in `backlog` with no worker.
 *   - Mark A `done`, then assert B finally gets picked up.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const MODEL = 'mock:happy-path'

async function authenticate(request: APIRequestContext): Promise<{ Authorization: string }> {
  // The server auto-bootstraps the admin from PECKBOARD_BOOTSTRAP_*
  // env vars at first start (see playwright.config.ts); we just log in.
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { Authorization: `Bearer ${token}` }
}

type Card = {
  id: string
  title: string
  step: string
  worker_session_id: string | null
  depends_on?: string[]
}

async function listCards(
  request: APIRequestContext,
  authHeader: { Authorization: string },
  projectId: string,
): Promise<Card[]> {
  const res = await request.get(`/api/projects/${projectId}/cards`, { headers: authHeader })
  expect(res.ok(), `list cards failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as Card[]
}

/** Poll until `predicate` holds for the named card, or time out. */
async function waitForCard(
  request: APIRequestContext,
  authHeader: { Authorization: string },
  projectId: string,
  cardId: string,
  predicate: (c: Card) => boolean,
  timeoutMs: number,
): Promise<Card> {
  const deadline = Date.now() + timeoutMs
  let last: Card | undefined
  while (Date.now() < deadline) {
    const cards = await listCards(request, authHeader, projectId)
    last = cards.find((c) => c.id === cardId)
    if (last && predicate(last)) return last
    await new Promise((r) => setTimeout(r, 1000))
  }
  throw new Error(`card ${cardId} never satisfied predicate; last: ${JSON.stringify(last)}`)
}

test('a dependent card is not picked up until its dependency is done', async ({ request }) => {
  const authHeader = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-deps-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-deps', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  // Two worker slots so B's gating is purely about the dependency, not
  // about A occupying the only slot.
  const projectRes = await request.post('/api/projects', {
    headers: authHeader,
    data: { name: 'deps project', folder_id: folder.id, worker_count: 2, model: MODEL },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  // Card A — the prerequisite.
  const aRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: authHeader,
    data: { title: 'A', description: 'prerequisite', step: 'backlog', priority: 0, model: MODEL },
  })
  expect(aRes.ok(), `create A failed: ${await aRes.text()}`).toBeTruthy()
  const cardA = (await aRes.json()) as Card

  // Card B — depends on A.
  const bRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: authHeader,
    data: {
      title: 'B',
      description: 'dependent',
      step: 'backlog',
      priority: 1,
      model: MODEL,
      depends_on: [cardA.id],
    },
  })
  expect(bRes.ok(), `create B failed: ${await bRes.text()}`).toBeTruthy()
  const cardB = (await bRes.json()) as Card
  expect(cardB.depends_on, 'B records its dependency on A').toEqual([cardA.id])

  // The orchestrator runs on a 5s tick; A should be picked up (moved to
  // in_progress) well within a generous window.
  await waitForCard(
    request,
    authHeader,
    project.id,
    cardA.id,
    (c) => c.step === 'in_progress',
    20_000,
  )

  // Give the orchestrator another full tick, then confirm B is STILL
  // gated: no worker, still in backlog — even though a slot is free.
  await new Promise((r) => setTimeout(r, 6_000))
  const bGated = (await listCards(request, authHeader, project.id)).find((c) => c.id === cardB.id)!
  expect(bGated.step, 'B stays in backlog while A is not done').toBe('backlog')
  expect(bGated.worker_session_id, 'B has no worker while A is not done').toBeFalsy()

  // Complete the dependency. Now B's only prerequisite is `done`.
  const doneRes = await request.put(`/api/projects/${project.id}/cards/${cardA.id}`, {
    headers: authHeader,
    data: { step: 'done' },
  })
  expect(doneRes.ok(), `mark A done failed: ${await doneRes.text()}`).toBeTruthy()

  // B should now be picked up by the orchestrator.
  await waitForCard(
    request,
    authHeader,
    project.id,
    cardB.id,
    (c) => c.step === 'in_progress',
    20_000,
  )
})
