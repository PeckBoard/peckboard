import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
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

async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; authHeader: { Authorization: string } }> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

test('per-project workflow instructions round-trip via the HTTP API', async ({ request }) => {
  const { authHeader } = await authenticate(request)

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

test('WorkflowInstructionsModal persists text via Edit Project and round-trips on re-open', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)

  // Seed a folder + project on the `fast-develop-software` workflow so
  // the modal has at least one customisable worker step (`in_progress`).
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-wf-instr-ui-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-wf-instr-ui', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: authHeader,
    data: {
      name: 'wf-instructions-ui project',
      folder_id: folder.id,
      worker_count: 1,
      workflow: 'fast-develop-software',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  await loadAppAt(page, token, `/projects/${project.id}`)

  // Open the project menu → Edit project → Edit workflow instructions….
  const openInstructionsModal = async () => {
    await page.getByRole('button', { name: 'Project menu' }).click()
    // Items inside the shared Dropdown carry `role="menuitem"` per the menu
    // a11y pattern, so query by that role (not `button`) — same shape as the
    // user-menu / change-password tests use for their dropdown items.
    await page.getByRole('menuitem', { name: 'Edit project' }).click()
    await page.getByRole('button', { name: 'Edit workflow instructions…' }).click()
    // Wait for the per-step editor to render — its textarea proves both
    // the modal is open AND the workflow registry + existing overrides
    // have loaded (the modal shows a "Loading current instructions…"
    // placeholder until then).
    await expect(
      page.locator('.workflow-instructions-modal').getByLabel('Your additional instructions'),
    ).toBeVisible()
  }

  // Both the EditProjectModal and the WorkflowInstructionsModal share
  // generic button labels ("Save"/"Cancel"), so scope every interaction
  // inside the workflow-instructions modal to its container class to
  // avoid matching the wrong layer.
  const wfModal = page.locator('.workflow-instructions-modal')

  await openInstructionsModal()
  // Picker defaults to `fast-develop-software` (it's the project's
  // workflow, passed via initialWorkflowId); set it explicitly so the
  // assertion isn't tied to picker ordering.
  await wfModal.getByLabel('Workflow').selectOption('fast-develop-software')

  // `fast-develop-software` has exactly one worker-running step
  // (`in_progress`), so there's only one "Your additional instructions"
  // textarea inside the modal at this point.
  const message = 'At the end, commit to master and push.'
  await wfModal.getByLabel('Your additional instructions').fill(message)
  await wfModal.getByRole('button', { name: 'Save' }).click()
  await expect(wfModal.getByText('Saved.')).toBeVisible()

  // Close the modal stack so we can re-open from scratch and prove the
  // value round-trips off the server, not just stale React state.
  await wfModal.getByRole('button', { name: 'Close' }).click()
  await page.getByRole('button', { name: 'Cancel' }).click()

  await openInstructionsModal()
  await wfModal.getByLabel('Workflow').selectOption('fast-develop-software')
  await expect(wfModal.getByLabel('Your additional instructions')).toHaveValue(message)

  // And the server agrees — list endpoint returns the same row.
  const listRes = await request.get(`/api/projects/${project.id}/workflow-instructions`, {
    headers: authHeader,
  })
  expect(listRes.ok()).toBeTruthy()
  const payload = (await listRes.json()) as {
    instructions: Array<{ workflow_id: string; step: string; instructions: string }>
  }
  expect(payload.instructions).toHaveLength(1)
  expect(payload.instructions[0]).toMatchObject({
    workflow_id: 'fast-develop-software',
    step: 'in_progress',
    instructions: message,
  })
})
