import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Worker sessions are owned by their card / project. The orchestrator
 * spawns them when a card is picked up and the project / card cascade
 * cleans them up when the parent goes away. A user who hits
 * `DELETE /api/sessions/:id` on one of these directly would leave the
 * parent card pointing at a vanished `worker_session_id` and bypass the
 * orchestrator's bookkeeping — so the route now refuses with 409.
 *
 * This spec drives the full pickup path: create a project with a
 * `mock:happy-path` card, wait for the orchestrator to spawn a worker,
 * then assert the rejection contract. The denormalized tab payload also
 * carries `is_worker` so the frontend can hide the "Delete session" tab
 * affordance up-front; we lock that in too.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

type CardRow = {
  id: string
  worker_session_id: string | null
  last_worker_session_id: string | null
}

async function waitForWorkerSession(
  request: APIRequestContext,
  auth: Record<string, string>,
  projectId: string,
  cardId: string,
): Promise<string> {
  // The orchestrator tick is ~5s. Poll for up to 30s; either the live
  // worker_session_id or the post-completion last_worker_session_id is
  // good enough — both are worker sessions the user could click into.
  const deadline = Date.now() + 30_000
  while (Date.now() < deadline) {
    const res = await request.get(`/api/projects/${projectId}/cards`, { headers: auth })
    expect(res.ok(), `list cards failed: ${await res.text()}`).toBeTruthy()
    const cards = (await res.json()) as CardRow[]
    const card = cards.find((c) => c.id === cardId)
    const sid = card?.worker_session_id ?? card?.last_worker_session_id ?? null
    if (sid) return sid
    await new Promise((r) => setTimeout(r, 500))
  }
  throw new Error('orchestrator never spawned a worker session within 30s')
}

test('DELETE /api/sessions/:id refuses worker sessions and the row survives', async ({
  request,
}) => {
  const { auth } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-worker-delete-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-worker-delete-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  // `mock:happy-path` finishes the scenario cleanly so the worker session
  // exists in the DB as `last_worker_session_id` even after the card
  // advances — exactly the state a user would click into via "View
  // Session" on a finished card.
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: 'worker delete guard',
      folder_id: folder.id,
      worker_count: 1,
      workflow: 'task',
      model: 'mock:happy-path',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: auth,
    data: { title: 'Pick me up', description: '', step: 'backlog', priority: 1 },
  })
  expect(cardRes.ok(), `create card failed: ${await cardRes.text()}`).toBeTruthy()
  const card = (await cardRes.json()) as { id: string }

  const workerSessionId = await waitForWorkerSession(request, auth, project.id, card.id)

  // Attempt the direct delete — backend must refuse with 409.
  const deleteRes = await request.delete(`/api/sessions/${workerSessionId}`, { headers: auth })
  expect(deleteRes.status(), 'worker-session delete must return 409').toBe(409)

  // The row is still there — the orchestrator's bookkeeping depends on
  // the FK continuing to resolve.
  const getRes = await request.get(`/api/sessions/${workerSessionId}`, { headers: auth })
  expect(getRes.ok(), 'worker session must survive the rejected delete').toBeTruthy()
  const session = (await getRes.json()) as { id: string; is_worker: boolean; project_id: string }
  expect(session.is_worker, 'session must still be flagged as a worker').toBe(true)
  expect(session.project_id).toBe(project.id)

  // The denormalized tab payload should expose `is_worker` so the
  // frontend can hide the "Delete session" context-menu entry without
  // having to fetch every session row individually.
  const upsertRes = await request.post('/api/me/tabs', {
    headers: auth,
    data: { item_type: 'session', item_id: workerSessionId },
  })
  expect(upsertRes.ok(), `tab upsert failed: ${await upsertRes.text()}`).toBeTruthy()
  const upserted = (await upsertRes.json()) as { is_worker?: boolean }
  expect(upserted.is_worker, 'tab payload should flag worker sessions').toBe(true)

  const listRes = await request.get('/api/me/tabs', { headers: auth })
  expect(listRes.ok()).toBeTruthy()
  const tabs = (await listRes.json()) as { item_id: string; is_worker?: boolean }[]
  const ours = tabs.find((t) => t.item_id === workerSessionId)
  expect(ours?.is_worker, 'GET listing should also flag worker tabs').toBe(true)
})
