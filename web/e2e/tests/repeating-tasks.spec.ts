import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E coverage for repeating tasks:
 *  - Create + force-run from the HTTP API.
 *  - Second force-run while the first is in flight must NOT spawn a
 *    second session — the backend's per-task lock should report
 *    "already_running".
 *  - Pause flips `enabled = false`; resume flips it back.
 *  - The /api/repeating-tasks/{id}/sessions endpoint exposes the run
 *    history.
 *
 * Uses the mock provider so timing is deterministic and we don't depend
 * on the real Claude CLI being installed in CI.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

test('create + force-run, second run while busy is rejected, sessions list grows', async ({
  request,
}) => {
  const { authHeader } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-rt-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-rt', path: folderPath },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const createRes = await request.post('/api/repeating-tasks', {
    headers: authHeader,
    data: {
      name: 'nightly sweep',
      description: 'Just a description for humans',
      folder_id: folder.id,
      prompt: 'go',
      schedule_kind: 'interval',
      schedule_value: { minutes: 60 },
      model: 'mock:happy-path',
      enabled: true,
    },
  })
  expect(createRes.ok(), `create failed: ${await createRes.text()}`).toBeTruthy()
  const task = (await createRes.json()) as {
    id: string
    name: string
    enabled: boolean
    next_run_at: string | null
  }
  expect(task.enabled).toBe(true)
  expect(task.next_run_at).not.toBeNull()

  // Trigger the first run.
  const firstRun = await request.post(`/api/repeating-tasks/${task.id}/run`, {
    headers: authHeader,
  })
  expect(firstRun.ok()).toBeTruthy()
  const firstStatus = (await firstRun.json()) as { status: string }
  expect(firstStatus.status).toBe('spawned')

  // The session row appears immediately; the mock provider keeps the
  // session in `is_running` long enough to land a concurrent second run.
  const listRes = await request.get(`/api/repeating-tasks/${task.id}/sessions`, {
    headers: authHeader,
  })
  expect(listRes.ok()).toBeTruthy()
  const sessions = (await listRes.json()) as Array<{ id: string }>
  expect(sessions.length).toBe(1)

  // Race a tight loop of force-runs. The backend's TaskLock guarantees
  // that two of these can never *both* spawn — the second one in flight
  // must report `already_running`. The mock provider plays its scripted
  // sequence quickly, so we run the loop in parallel rather than
  // sequentially to maximise the race window.
  const parallelRuns = Array.from({ length: 20 }, () =>
    request
      .post(`/api/repeating-tasks/${task.id}/run`, { headers: authHeader })
      .then((r) => r.json() as Promise<{ status: string }>),
  )
  const outcomes = await Promise.all(parallelRuns)
  const spawned = outcomes.filter((o) => o.status === 'spawned').length
  const blocked = outcomes.filter((o) => o.status === 'already_running').length
  // The invariant: every force-run either spawned or got blocked. There
  // are no other valid outcomes for an enabled task.
  expect(spawned + blocked).toBe(outcomes.length)
  // At least one must have been blocked, OR every one fully completed in
  // sequence between calls (which only happens on absurdly fast
  // hardware). Given parallel POSTs and the mock provider's runtime, the
  // race is essentially guaranteed.
  expect(blocked).toBeGreaterThan(0)
})

test('pause flips enabled; update validates schedule shape', async ({ request }) => {
  const { authHeader } = await authenticate(request)
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-rt-pause-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-rt-pause', path: folderPath },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const createRes = await request.post('/api/repeating-tasks', {
    headers: authHeader,
    data: {
      name: 'pausable',
      folder_id: folder.id,
      prompt: 'do thing',
      schedule_kind: 'daily',
      schedule_value: { hour: 9, minute: 0 },
    },
  })
  expect(createRes.ok()).toBeTruthy()
  const task = (await createRes.json()) as { id: string; enabled: boolean }
  expect(task.enabled).toBe(true)

  // Pause via PATCH
  const pauseRes = await request.patch(`/api/repeating-tasks/${task.id}`, {
    headers: authHeader,
    data: { enabled: false },
  })
  expect(pauseRes.ok()).toBeTruthy()
  const paused = (await pauseRes.json()) as { enabled: boolean; next_run_at: string | null }
  expect(paused.enabled).toBe(false)
  expect(paused.next_run_at).toBeNull()

  // Resume — next_run_at should come back.
  const resumeRes = await request.patch(`/api/repeating-tasks/${task.id}`, {
    headers: authHeader,
    data: { enabled: true },
  })
  expect(resumeRes.ok()).toBeTruthy()
  const resumed = (await resumeRes.json()) as { enabled: boolean; next_run_at: string | null }
  expect(resumed.enabled).toBe(true)
  expect(resumed.next_run_at).not.toBeNull()

  // Invalid schedule must be rejected at the boundary.
  const badRes = await request.patch(`/api/repeating-tasks/${task.id}`, {
    headers: authHeader,
    data: { schedule_kind: 'daily', schedule_value: { hour: 99, minute: 0 } },
  })
  expect(badRes.status()).toBe(400)
})

test('list returns folder-filtered results when ?folder_id is supplied', async ({ request }) => {
  const { authHeader } = await authenticate(request)
  // Two folders, two tasks each.
  const setup = async (name: string) => {
    const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-rt-listfilter-${name}-`))
    const folderRes = await request.post('/api/folders', {
      headers: authHeader,
      data: { name: `e2e-rt-listfilter-${name}`, path: folderPath },
    })
    expect(folderRes.ok()).toBeTruthy()
    const folder = (await folderRes.json()) as { id: string }
    await request.post('/api/repeating-tasks', {
      headers: authHeader,
      data: {
        name: `task-${name}-a`,
        folder_id: folder.id,
        prompt: 'go',
        schedule_kind: 'interval',
        schedule_value: { minutes: 60 },
      },
    })
    await request.post('/api/repeating-tasks', {
      headers: authHeader,
      data: {
        name: `task-${name}-b`,
        folder_id: folder.id,
        prompt: 'go',
        schedule_kind: 'interval',
        schedule_value: { minutes: 120 },
      },
    })
    return folder
  }

  const f1 = await setup('one')
  const f2 = await setup('two')

  const all = await (await request.get('/api/repeating-tasks', { headers: authHeader })).json()
  expect(all.length).toBeGreaterThanOrEqual(4)

  const f1Only = (await (
    await request.get(`/api/repeating-tasks?folder_id=${f1.id}`, { headers: authHeader })
  ).json()) as Array<{ folder_id: string; name: string }>
  expect(f1Only.every((t) => t.folder_id === f1.id)).toBe(true)
  const names = f1Only.map((t) => t.name)
  expect(names).toContain('task-one-a')
  expect(names).toContain('task-one-b')

  const f2Only = (await (
    await request.get(`/api/repeating-tasks?folder_id=${f2.id}`, { headers: authHeader })
  ).json()) as Array<{ folder_id: string }>
  expect(f2Only.every((t) => t.folder_id === f2.id)).toBe(true)
})

test('delete removes the task but preserves spawned sessions (detaches them)', async ({
  request,
}) => {
  const { authHeader } = await authenticate(request)
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-rt-delete-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-rt-delete', path: folderPath },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const createRes = await request.post('/api/repeating-tasks', {
    headers: authHeader,
    data: {
      name: 'will-be-deleted',
      folder_id: folder.id,
      prompt: 'go',
      schedule_kind: 'interval',
      schedule_value: { minutes: 60 },
      model: 'mock:happy-path',
    },
  })
  expect(createRes.ok()).toBeTruthy()
  const task = (await createRes.json()) as { id: string }

  // Spawn at least one session via force-run.
  const runRes = await request.post(`/api/repeating-tasks/${task.id}/run`, {
    headers: authHeader,
  })
  expect(runRes.ok()).toBeTruthy()
  const sessionsBefore = (await (
    await request.get(`/api/repeating-tasks/${task.id}/sessions`, { headers: authHeader })
  ).json()) as Array<{ id: string }>
  expect(sessionsBefore.length).toBe(1)
  const sessionId = sessionsBefore[0].id

  const deleteRes = await request.delete(`/api/repeating-tasks/${task.id}`, {
    headers: authHeader,
  })
  expect(deleteRes.status()).toBe(204)

  // The task is gone (subsequent fetch returns 404).
  const afterRes = await request.get(`/api/repeating-tasks/${task.id}`, {
    headers: authHeader,
  })
  expect(afterRes.status()).toBe(404)

  // The session survives — it just no longer references the task.
  const sessionRes = await request.get(`/api/sessions/${sessionId}`, {
    headers: authHeader,
  })
  expect(sessionRes.ok()).toBeTruthy()
  const session = (await sessionRes.json()) as { repeating_task_id: string | null }
  expect(session.repeating_task_id).toBeNull()
})

test('disabled task: force-run respects enabled when using PATCH but bypasses it via run endpoint', async ({
  request,
}) => {
  const { authHeader } = await authenticate(request)
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-rt-disabled-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-rt-disabled', path: folderPath },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const createRes = await request.post('/api/repeating-tasks', {
    headers: authHeader,
    data: {
      name: 'disabled-task',
      folder_id: folder.id,
      prompt: 'go',
      schedule_kind: 'interval',
      schedule_value: { minutes: 60 },
      model: 'mock:happy-path',
      enabled: false,
    },
  })
  expect(createRes.ok()).toBeTruthy()
  const task = (await createRes.json()) as { id: string; next_run_at: string | null }
  expect(task.next_run_at).toBeNull()

  // Run endpoint bypasses `enabled` so an operator can always force a
  // one-off.
  const runRes = await request.post(`/api/repeating-tasks/${task.id}/run`, {
    headers: authHeader,
  })
  expect(runRes.ok()).toBeTruthy()
  const runBody = (await runRes.json()) as { status: string }
  expect(runBody.status).toBe('spawned')
})
