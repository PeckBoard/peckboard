import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * The shared `review` step: every workflow runs it right before `done`.
 *
 * 1. A `mock:mcp` worker runs the card's `in_progress` step and calls
 *    `finish_card` — the card lands on `review`, not `done`.
 * 2. The orchestrator spawns a FRESH reviewer session on the project's
 *    reviewer model (`mock:mcp@acct2` — same mock scenario, different model
 *    id). It lists the worker sessions: the implementer is a separate row.
 *    (The reviewer prompt's handoff section is covered by the Rust test
 *    `review_prompt_points_at_the_previous_worker_and_its_claims`.)
 * 3. The reviewer files a gap card with `create_card` and calls
 *    `finish_card` — the origin card lands on `done`, the gap card shows up
 *    on the board.
 *
 * `mock:mcp` runs every ```mcp block in its prompt, so the per-step
 * behaviour is scripted through the project's per-step workflow
 * instructions (appended only to that step's worker prompt). The gap card is
 * filed blocked so no worker picks it up and loops the flow.
 *
 * A second test drives the Edit Project form: the review toggle, the
 * reviewer model picker (with "Same as project"), and reviewer effort.
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

type CardRow = {
  id: string
  title: string
  step: string
  blocked: boolean
  workflow: string
  worker_session_id: string | null
  last_worker_session_id: string | null
}

async function listCards(
  request: APIRequestContext,
  auth: Record<string, string>,
  projectId: string,
): Promise<CardRow[]> {
  const res = await request.get(`/api/projects/${projectId}/cards`, { headers: auth })
  expect(res.ok()).toBeTruthy()
  return (await res.json()) as CardRow[]
}

async function createProject(
  request: APIRequestContext,
  auth: Record<string, string>,
  data: Record<string, unknown>,
): Promise<{ id: string }> {
  const stamp = Date.now()
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-review-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-review-${stamp}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: { name: `review project ${stamp}`, folder_id: folder.id, workflow: 'task', ...data },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  return (await projectRes.json()) as { id: string }
}

const mcp = (tool: string, args: Record<string, unknown>) =>
  '```mcp\n' + JSON.stringify({ tool, args }) + '\n```'

test('a finished card is reviewed by a fresh session on the reviewer model, which files gaps', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const project = await createProject(request, auth, {
    worker_count: 1,
    model: 'mock:mcp',
    review_model: 'mock:mcp@acct2',
  })

  const gapTitle = `Gap: widget docs missing ${Date.now()}`
  for (const [step, instructions] of [
    ['in_progress', mcp('finish_card', { summary: 'Built the widget in src/widget.rs.' })],
    [
      'review',
      [
        mcp('list_worker_sessions', {}),
        mcp('create_card', {
          title: gapTitle,
          description: 'Origin card is missing its docs requirement.',
          workflow: 'task',
          block_reason: 'review gap (e2e): held for triage',
        }),
        mcp('finish_card', { summary: 'Verified widget; filed 1 gap card for docs.' }),
      ].join('\n'),
    ],
  ]) {
    const res = await request.put(`/api/projects/${project.id}/workflow-instructions`, {
      headers: auth,
      data: { workflow_id: 'task', step, instructions },
    })
    expect(res.ok(), `set ${step} instructions failed: ${await res.text()}`).toBeTruthy()
  }

  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: auth,
    data: {
      title: 'Build the widget',
      description: 'Acceptance: widget exists and is documented.',
      step: 'backlog',
      priority: 1,
      workflow: 'task',
    },
  })
  expect(cardRes.ok(), `create card failed: ${await cardRes.text()}`).toBeTruthy()
  const card = (await cardRes.json()) as { id: string }

  // The card ends on `done` (via `review`, proven by the reviewer session
  // asserted below).
  await expect
    .poll(
      async () => (await listCards(request, auth, project.id)).find((x) => x.id === card.id)?.step,
      { timeout: 45_000, intervals: [150] },
    )
    .toBe('done')

  // The last worker is the reviewer: a separate review session on the
  // reviewer model.
  const done = (await listCards(request, auth, project.id)).find((x) => x.id === card.id)!
  const reviewerId = done.last_worker_session_id
  expect(reviewerId, 'reviewer session recorded on the card').toBeTruthy()
  const reviewerRes = await request.get(`/api/sessions/${reviewerId}`, { headers: auth })
  expect(reviewerRes.ok()).toBeTruthy()
  const reviewer = (await reviewerRes.json()) as { model: string; name: string }
  expect(reviewer.name).toMatch(/^review: /)
  expect(reviewer.model).toBe('mock:mcp@acct2')

  // The reviewer's `list_worker_sessions` result (recorded in its
  // transcript) shows the implementer as a different session.
  const eventsRes = await request.get(`/api/sessions/${reviewerId}/events?limit=1000`, {
    headers: auth,
  })
  expect(eventsRes.ok()).toBeTruthy()
  // Tool outputs are JSON strings inside the event data; decode any that
  // carry the `workers` listing.
  type Worker = { session_id: string; session_name: string }
  const workers: Worker[] = []
  const collect = (v: unknown): void => {
    if (typeof v === 'string' && v.includes('"workers"')) {
      try {
        const parsed = JSON.parse(v) as { workers?: Worker[] }
        if (Array.isArray(parsed.workers)) workers.push(...parsed.workers)
      } catch {
        // not JSON — ignore
      }
    } else if (v && typeof v === 'object') {
      Object.values(v as Record<string, unknown>).forEach(collect)
    }
  }
  collect(await eventsRes.json())
  const implementerId = workers.find(
    (w) => w.session_name === 'worker: Build the widget',
  )?.session_id
  expect(implementerId, 'implementer session visible to the reviewer').toBeTruthy()
  expect(implementerId).not.toBe(reviewerId)

  const implementerRes = await request.get(`/api/sessions/${implementerId}`, { headers: auth })
  expect(implementerRes.ok()).toBeTruthy()
  const implementer = (await implementerRes.json()) as { model: string; name: string }
  expect(implementer.name).toMatch(/^worker: /)
  expect(implementer.model).toBe('mock:mcp')

  // The reviewer's gap card exists, blocked, in the same project/workflow.
  const gap = (await listCards(request, auth, project.id)).find((x) => x.title === gapTitle)
  expect(gap, 'reviewer filed the gap card').toBeTruthy()
  expect(gap!.step).toBe('backlog')
  expect(gap!.blocked).toBe(true)
  expect(gap!.workflow).toBe('task')

  // And the board shows it.
  await loadAt(page, token, `/projects/${project.id}`)
  await expect(page.getByText(gapTitle)).toBeVisible({ timeout: 10_000 })
  await expect(page.getByText('Build the widget')).toBeVisible()
})

test('Edit Project sets the reviewer toggle, model, and effort', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  // No cards, so no worker ever spawns; worker_count stays in the form's 1-10.
  const project = await createProject(request, auth, { worker_count: 1, model: 'mock:echo' })

  const fetchProject = async () => {
    const res = await request.get(`/api/projects/${project.id}`, { headers: auth })
    expect(res.ok()).toBeTruthy()
    return ((await res.json()) as { project: Record<string, unknown> }).project
  }
  const initial = await fetchProject()
  expect(initial.review_enabled).toBe(true)
  expect(initial.review_model).toBeNull()

  await loadAt(page, token, `/projects/${project.id}`)
  const openEdit = async () => {
    await page.getByRole('button', { name: 'Project menu' }).click()
    await page.getByRole('menuitem', { name: 'Edit project' }).click()
  }

  await openEdit()
  const toggle = page.getByTestId('edit-project-review-enabled')
  await expect(toggle).toBeChecked()
  // Reviewer model is a searchable picker defaulting to "Same as project".
  const picker = page.getByTestId('edit-project-review-model')
  await expect(picker).toContainText('Same as project')
  await picker.click()
  await page.getByTestId('edit-project-review-model-search').fill('mock:happy-path')
  await page.getByTestId('edit-project-review-model-option-mock:happy-path').click()
  await expect(picker).not.toContainText('Same as project')
  await expect(page.getByTestId('edit-project-review-effort')).toHaveValue('')
  await page.getByRole('button', { name: 'Save' }).click()
  await expect(toggle).toBeHidden()

  await expect.poll(async () => (await fetchProject()).review_model).toBe('mock:happy-path')

  // Turning review off hides the reviewer fields and persists.
  await openEdit()
  await expect(page.getByTestId('edit-project-review-model')).toContainText('happy path')
  await page.getByTestId('edit-project-review-enabled').uncheck()
  await expect(page.getByTestId('edit-project-review-model')).toBeHidden()
  await page.getByRole('button', { name: 'Save' }).click()
  await expect(page.getByTestId('edit-project-review-enabled')).toBeHidden()
  await expect.poll(async () => (await fetchProject()).review_enabled).toBe(false)
})
