import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Verifies that the `worktree_isolation` toggle on a project round-trips
 * correctly through create → read → update → read. This is a pure-API
 * test — it exercises the DB column, migration, route handler, and JSON
 * serialization without needing a running worker.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { Authorization: `Bearer ${token}` }
}

async function makeFolder(request: APIRequestContext, auth: Record<string, string>) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-worktree-'))
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-worktree-${Date.now()}`, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as { id: string }
}

test('worktree_isolation toggle persists on create and update', async ({ request }) => {
  const auth = await authenticate(request)
  const folder = await makeFolder(request, auth)

  // Create project with worktree_isolation: true
  const createRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: 'e2e-worktree-isolation',
      folder_id: folder.id,
      worker_count: 1,
      workflow: 'task',
      model: 'mock:happy-path',
      worktree_isolation: true,
    },
  })
  expect(createRes.ok(), `create project failed: ${await createRes.text()}`).toBeTruthy()
  const created = (await createRes.json()) as { id: string; worktree_isolation: boolean }
  expect(created.worktree_isolation).toBe(true)

  // Read back via list and verify field is persisted
  const listRes = await request.get('/api/projects', { headers: auth })
  expect(listRes.ok()).toBeTruthy()
  const projects = (await listRes.json()) as Array<{ id: string; worktree_isolation: boolean }>
  const found = projects.find((p) => p.id === created.id)
  expect(found, 'project not found in list').toBeDefined()
  expect(found!.worktree_isolation).toBe(true)

  // Update to false and verify (route is PUT, not PATCH)
  const updateRes = await request.put(`/api/projects/${created.id}`, {
    headers: auth,
    data: { worktree_isolation: false },
  })
  expect(updateRes.ok(), `update project failed: ${await updateRes.text()}`).toBeTruthy()
  const updated = (await updateRes.json()) as { worktree_isolation: boolean }
  expect(updated.worktree_isolation).toBe(false)

  // Read back again to confirm persistence
  const listRes2 = await request.get('/api/projects', { headers: auth })
  const projects2 = (await listRes2.json()) as Array<{ id: string; worktree_isolation: boolean }>
  const found2 = projects2.find((p) => p.id === created.id)
  expect(found2!.worktree_isolation).toBe(false)
})

test('worktree_isolation defaults to false when not provided', async ({ request }) => {
  const auth = await authenticate(request)
  const folder = await makeFolder(request, auth)

  const createRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: 'e2e-worktree-default',
      folder_id: folder.id,
      worker_count: 1,
      workflow: 'task',
      model: 'mock:happy-path',
      // no worktree_isolation field
    },
  })
  expect(createRes.ok(), `create project failed: ${await createRes.text()}`).toBeTruthy()
  const created = (await createRes.json()) as { worktree_isolation: boolean }
  expect(created.worktree_isolation).toBe(false)
})
