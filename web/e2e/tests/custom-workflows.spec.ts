import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * User-defined workflows: create one in Settings → Workflows (with a step
 * name that doesn't exist on any built-in workflow), pick it in the card
 * form's WorkflowSelect (tagged "custom" alongside the built-ins), and
 * confirm the orchestrator actually resolves it end-to-end — a card
 * assigned to it advances into the CUSTOM step name, which could only
 * happen if the orchestrator read the workflow out of the registry
 * (not just the picker) when computing step order.
 *
 * The mock provider's `happy-path` scenario runs a scripted event
 * sequence and ignores the prompt text it's given, so a distinct step
 * NAME (rather than trying to inspect the worker's prompt) is what makes
 * this assertion possible with a deterministic mock model. Backend
 * coverage for the instructions text itself (the part the mock model
 * can't surface) lives in `workflow::tests::registry_merges_builtins_and_custom_workflows`.
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

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

async function waitForCardStep(
  request: APIRequestContext,
  auth: Record<string, string>,
  projectId: string,
  cardId: string,
  step: string,
  timeoutMs: number,
): Promise<void> {
  const deadline = Date.now() + timeoutMs
  let lastStep = ''
  while (Date.now() < deadline) {
    const res = await request.get(`/api/projects/${projectId}/cards`, { headers: auth })
    expect(res.ok()).toBeTruthy()
    const cards = (await res.json()) as Array<{ id: string; step: string }>
    const card = cards.find((c) => c.id === cardId)
    if (card) {
      lastStep = card.step
      if (card.step === step) return
    }
    await new Promise((r) => setTimeout(r, 500))
  }
  throw new Error(`timed out waiting for card step '${step}', last seen '${lastStep}'`)
}

test('create custom workflow, assign to a card, orchestrator resolves its custom step', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)

  const customStepName = `custom_step_${Date.now()}`
  const workflowName = `E2E Custom Flow ${Date.now()}`

  // ── 1. Create the custom workflow via Settings → Workflows ─────────
  await loadAt(page, token, '/settings')
  await page.getByTestId('settings-nav-workflows').click()
  const section = page.getByTestId('custom-workflows-section')
  await expect(section).toBeVisible({ timeout: 10_000 })

  await page.getByTestId('workflow-new').click()
  const editor = page.getByTestId('workflow-editor')
  await expect(editor).toBeVisible()
  await page.getByTestId('workflow-name-input').fill(workflowName)
  await page.getByTestId('workflow-description-input').fill('e2e custom workflow')
  // Step 1 is the only non-terminal step in the default 3-step draft.
  // Give it a step name that no built-in workflow uses, so landing on it
  // can only mean the orchestrator resolved THIS workflow.
  await page.getByTestId('workflow-step-name-1').fill(customStepName)
  await page.getByTestId('workflow-step-instructions-1').fill('Do the custom thing.')
  await page.getByTestId('workflow-save').click()
  await expect(editor).toBeHidden({ timeout: 10_000 })

  const row = page.getByTestId('custom-workflows-list').locator('li', { hasText: workflowName })
  await expect(row).toBeVisible()
  await expect(row).toContainText('custom')

  const listRes = await request.get('/api/workflows', { headers: auth })
  expect(listRes.ok()).toBeTruthy()
  const { workflows } = (await listRes.json()) as {
    workflows: Array<{ id: string; name: string; source: string; steps: Array<{ step: string }> }>
  }
  const created = workflows.find((w) => w.name === workflowName)
  expect(created, 'created workflow present in /api/workflows').toBeTruthy()
  expect(created!.source).toBe('custom')
  expect(created!.steps.map((s) => s.step)).toEqual(['backlog', customStepName, 'done'])
  const workflowId = created!.id

  // ── 2. Assign it to a new card via the card form's WorkflowSelect ──
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-custom-wf-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-custom-wf-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: `custom workflow project ${Date.now()}`,
      folder_id: folder.id,
      worker_count: 1,
      workflow: 'task',
      model: 'mock:happy-path',
    },
  })
  expect(projectRes.ok()).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  await page.goto(`/projects/${project.id}`)
  await page.getByRole('button', { name: 'Add Card' }).click()
  const modal = page.locator('.modal').filter({ hasText: 'New Card' })
  await expect(modal).toBeVisible({ timeout: 10_000 })
  await modal.locator('input.form-input').first().fill('Uses custom workflow')

  await modal.locator('#card-workflow').click()
  const wfOption = page.locator('.dropdown-item').filter({ hasText: workflowName })
  await expect(wfOption).toBeVisible({ timeout: 5_000 })
  await expect(wfOption.locator('.dropdown-item-hint')).toHaveText('custom')
  await wfOption.click()

  await modal.getByRole('button', { name: 'Create Card' }).click()
  await expect(modal).toBeHidden({ timeout: 10_000 })

  const cardsRes = await request.get(`/api/projects/${project.id}/cards`, { headers: auth })
  expect(cardsRes.ok()).toBeTruthy()
  const cards = (await cardsRes.json()) as Array<{ id: string; title: string; workflow: string }>
  const card = cards.find((c) => c.title === 'Uses custom workflow')
  expect(card, 'created card present').toBeTruthy()
  expect(card!.workflow).toBe(workflowId)

  // ── 3. The orchestrator resolves the custom workflow: the card
  //      advances off `backlog` straight into the CUSTOM step name ─────
  await waitForCardStep(request, auth, project.id, card!.id, customStepName, 20_000)
})

/**
 * Editing a workflow step that an in-flight card is currently sitting on
 * must be refused (409) instead of silently stranding the card: before the
 * guard, that card's next `complete_step` found no matching step and
 * jumped straight to `done`, skipping every gate in between. The Workflows
 * editor surfaces the backend message in its `form-error`.
 */
test('editing a step an in-flight card sits on is rejected with the reason shown', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)

  const stamp = Date.now()
  const reviewStep = `review_gate_${stamp}`
  const workflowName = `E2E Gated Flow ${stamp}`

  const wfRes = await request.post('/api/workflows', {
    headers: auth,
    data: {
      name: workflowName,
      description: 'e2e gated workflow',
      steps: [
        { step: 'backlog', instructions: '' },
        { step: 'execution', instructions: 'Do the thing.' },
        { step: reviewStep, instructions: 'Review the thing.' },
        { step: 'done', instructions: '' },
      ],
    },
  })
  expect(wfRes.ok(), `create workflow failed: ${await wfRes.text()}`).toBeTruthy()
  const workflowId = ((await wfRes.json()) as { id: string }).id

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-gated-wf-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-gated-wf-${stamp}`, path: folderPath },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: `gated workflow project ${stamp}`,
      folder_id: folder.id,
      worker_count: 1,
      workflow: workflowId,
      model: 'mock:happy-path',
    },
  })
  expect(projectRes.ok()).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  // Blocked, so no worker picks it up and moves it off the review step
  // while the edit is being attempted.
  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: auth,
    data: {
      title: 'Sitting on the review gate',
      description: '',
      step: reviewStep,
      priority: 1,
      workflow: workflowId,
      blocked: true,
      block_reason: 'held for the e2e edit guard',
    },
  })
  expect(cardRes.ok(), `create card failed: ${await cardRes.text()}`).toBeTruthy()
  const card = (await cardRes.json()) as { id: string }

  const seeded = await request.get(`/api/projects/${project.id}/cards`, { headers: auth })
  expect(seeded.ok()).toBeTruthy()
  const seededCards = (await seeded.json()) as Array<{ id: string; step: string }>
  expect(seededCards.find((c) => c.id === card.id)?.step).toBe(reviewStep)

  // The API refuses the rename outright …
  const renameBody = {
    name: workflowName,
    description: 'e2e gated workflow',
    steps: [
      { step: 'backlog', instructions: '' },
      { step: 'execution', instructions: 'Do the thing.' },
      { step: `${reviewStep}_renamed`, instructions: 'Review the thing.' },
      { step: 'done', instructions: '' },
    ],
  }
  const apiPut = await request.put(`/api/workflows/${workflowId}`, {
    headers: auth,
    data: renameBody,
  })
  expect(apiPut.status(), `PUT body: ${await apiPut.text()}`).toBe(409)

  // ── Rename the step the card is on, in Settings → Workflows ────────
  await loadAt(page, token, '/settings')
  await page.getByTestId('settings-nav-workflows').click()
  await expect(page.getByTestId('custom-workflows-section')).toBeVisible({ timeout: 10_000 })

  await page.getByTestId(`workflow-edit-${workflowId}`).click()
  const editor = page.getByTestId('workflow-editor')
  await expect(editor).toBeVisible()
  await page.getByTestId('workflow-step-name-2').fill(`${reviewStep}_renamed`)
  await page.getByTestId('workflow-save').click()

  // Rejected: the editor stays open and the 409 reason is shown.
  const formError = page.locator('.form-error')
  await expect(formError).toContainText(reviewStep, { timeout: 10_000 })
  await expect(editor).toBeVisible()

  // The card never moved.
  const cardsRes = await request.get(`/api/projects/${project.id}/cards`, { headers: auth })
  expect(cardsRes.ok()).toBeTruthy()
  const cards = (await cardsRes.json()) as Array<{ id: string; step: string }>
  expect(cards.find((c) => c.id === card.id)?.step).toBe(reviewStep)

  // An edit that keeps that step still saves.
  await page.getByTestId('workflow-step-name-2').fill(reviewStep)
  await page.getByTestId('workflow-description-input').fill('e2e gated workflow, edited')
  await page.getByTestId('workflow-save').click()
  await expect(editor).toBeHidden({ timeout: 10_000 })
})
