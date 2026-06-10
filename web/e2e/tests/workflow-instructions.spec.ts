import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * End-to-end test for per-project workflow-instruction overrides.
 *
 * A project owner can attach additional text to any worker-running step
 * of any workflow (e.g. "commit to master and push" on `in_progress`
 * for `fast-develop-software`). The text is appended below the built-in
 * step prompt — both apply.
 *
 * This spec drives just the HTTP layer because that's where the
 * round-trip lives:
 *
 *   1. Create a project on the `fast-develop-software` workflow.
 *   2. PUT an override for (workflow_id, step) and GET it back.
 *   3. PUT empty `instructions` and verify the override is removed.
 *   4. Validate the (workflow, step) pair is checked: unknown step
 *      rejected, terminal `done` step rejected, unknown workflow rejected.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext): Promise<{ Authorization: string }> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { Authorization: `Bearer ${token}` }
}

test('per-project workflow instructions round-trip via the HTTP API', async ({ request }) => {
  const authHeader = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-wf-instr-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-wf-instr', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: authHeader,
    data: {
      name: 'workflow-instructions project',
      folder_id: folder.id,
      worker_count: 1,
      workflow: 'fast-develop-software',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  // Initially there are no overrides.
  let listRes = await request.get(`/api/projects/${project.id}/workflow-instructions`, {
    headers: authHeader,
  })
  expect(listRes.ok(), `list failed: ${await listRes.text()}`).toBeTruthy()
  let payload = (await listRes.json()) as { instructions: unknown[] }
  expect(payload.instructions).toEqual([])

  // Upsert: attach instructions to the `in_progress` step.
  const upsertRes = await request.put(`/api/projects/${project.id}/workflow-instructions`, {
    headers: authHeader,
    data: {
      workflow_id: 'fast-develop-software',
      step: 'in_progress',
      instructions: 'At the end, commit to master and push.',
    },
  })
  expect(upsertRes.ok(), `upsert failed: ${await upsertRes.text()}`).toBeTruthy()
  const upsertBody = (await upsertRes.json()) as {
    workflow_id: string
    step: string
    instructions: string
  }
  expect(upsertBody.instructions).toBe('At the end, commit to master and push.')
  expect(upsertBody.workflow_id).toBe('fast-develop-software')
  expect(upsertBody.step).toBe('in_progress')

  // The list endpoint surfaces it.
  listRes = await request.get(`/api/projects/${project.id}/workflow-instructions`, {
    headers: authHeader,
  })
  expect(listRes.ok()).toBeTruthy()
  payload = (await listRes.json()) as {
    instructions: Array<{ workflow_id: string; step: string; instructions: string }>
  }
  expect(payload.instructions).toHaveLength(1)
  expect(payload.instructions[0].workflow_id).toBe('fast-develop-software')
  expect(payload.instructions[0].step).toBe('in_progress')
  expect(payload.instructions[0].instructions).toBe('At the end, commit to master and push.')

  // Validation: terminal `done` step has no worker prompt; rejected.
  const doneRes = await request.put(`/api/projects/${project.id}/workflow-instructions`, {
    headers: authHeader,
    data: {
      workflow_id: 'fast-develop-software',
      step: 'done',
      instructions: 'nope',
    },
  })
  expect(doneRes.status()).toBe(400)

  // Validation: unknown step rejected.
  const unknownStepRes = await request.put(`/api/projects/${project.id}/workflow-instructions`, {
    headers: authHeader,
    data: {
      workflow_id: 'fast-develop-software',
      step: 'in-progress', // wrong spelling (dash vs underscore)
      instructions: 'nope',
    },
  })
  expect(unknownStepRes.status()).toBe(400)

  // Validation: unknown workflow rejected.
  const unknownWfRes = await request.put(`/api/projects/${project.id}/workflow-instructions`, {
    headers: authHeader,
    data: {
      workflow_id: 'not-a-real-workflow',
      step: 'in_progress',
      instructions: 'nope',
    },
  })
  expect(unknownWfRes.status()).toBe(400)

  // Clear: empty instructions delete the override.
  const clearRes = await request.put(`/api/projects/${project.id}/workflow-instructions`, {
    headers: authHeader,
    data: {
      workflow_id: 'fast-develop-software',
      step: 'in_progress',
      instructions: '   \n  ',
    },
  })
  expect(clearRes.ok(), `clear failed: ${await clearRes.text()}`).toBeTruthy()

  listRes = await request.get(`/api/projects/${project.id}/workflow-instructions`, {
    headers: authHeader,
  })
  expect(listRes.ok()).toBeTruthy()
  payload = (await listRes.json()) as { instructions: unknown[] }
  expect(payload.instructions).toEqual([])

  // Unknown project surfaces a 404 (no orphan rows).
  const orphanRes = await request.put('/api/projects/does-not-exist/workflow-instructions', {
    headers: authHeader,
    data: {
      workflow_id: 'fast-develop-software',
      step: 'in_progress',
      instructions: 'x',
    },
  })
  expect(orphanRes.status()).toBe(404)
})
