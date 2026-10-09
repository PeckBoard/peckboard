import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Card session history + review summary.
 *
 * A `mock:mcp` implementer finishes the card (→ `review`), a fresh reviewer
 * passes it (→ `done`). Then, in the UI:
 *
 * 1. The card tile carries a verdict badge; the card detail shows the
 *    Review section with the verdict and the reviewer's summary.
 * 2. "Sessions (N)" opens the run history: the implementation and review
 *    runs, each with its summary.
 * 3. Opening the implementation run lands on its transcript, which is
 *    sealed: read-only banner, no composer.
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

const mcp = (tool: string, args: Record<string, unknown>) =>
  '```mcp\n' + JSON.stringify({ tool, args }) + '\n```'

test('card detail shows the review verdict and the run history with sealed transcripts', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const stamp = Date.now()
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-history-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-history-${stamp}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: `history project ${stamp}`,
      folder_id: folder.id,
      workflow: 'task',
      worker_count: 1,
      model: 'mock:mcp',
      review_model: 'mock:mcp@acct2',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  const implSummary = 'Built the gizmo in src/gizmo.rs.'
  const reviewSummary = 'Verified the gizmo end to end; no gaps.'
  for (const [step, instructions] of [
    ['in_progress', mcp('finish_card', { summary: implSummary })],
    ['review', mcp('finish_card', { summary: reviewSummary })],
  ]) {
    const res = await request.put(`/api/projects/${project.id}/workflow-instructions`, {
      headers: auth,
      data: { workflow_id: 'task', step, instructions },
    })
    expect(res.ok(), `set ${step} instructions failed: ${await res.text()}`).toBeTruthy()
  }

  const cardTitle = `Build the gizmo ${stamp}`
  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: auth,
    data: {
      title: cardTitle,
      description: 'Gizmo exists.',
      step: 'backlog',
      priority: 1,
      workflow: 'task',
    },
  })
  expect(cardRes.ok(), `create card failed: ${await cardRes.text()}`).toBeTruthy()
  const card = (await cardRes.json()) as { id: string }

  type CardRow = { id: string; step: string; review_verdict?: string | null }
  const fetchCard = async () => {
    const res = await request.get(`/api/projects/${project.id}/cards`, { headers: auth })
    return ((await res.json()) as CardRow[]).find((c) => c.id === card.id)
  }
  await expect
    .poll(async () => (await fetchCard())?.step, { timeout: 45_000, intervals: [150] })
    .toBe('done')
  await expect.poll(async () => (await fetchCard())?.review_verdict).toBe('pass')

  await loadAt(page, token, `/projects/${project.id}`)
  const tile = page.locator('.kanban-card', { hasText: cardTitle })
  await expect(tile.getByTestId('card-verdict-badge')).toHaveText('Passed', { timeout: 10_000 })

  // Card detail → Review section.
  await tile.locator('.kanban-card-title').click()
  const review = page.getByTestId('card-review-summary')
  await expect(review.getByTestId('card-review-verdict')).toHaveText('Passed')
  // Reviewer model rides on the card row (`reviewer_model`).
  await expect(review.locator('.card-run-model')).toContainText('mock:mcp')
  await expect(review).toContainText(reviewSummary)

  // Sessions (N) → run history.
  const sessionsBtn = page.getByTestId('card-sessions-btn')
  await expect(sessionsBtn).toHaveText(/Sessions \([2-9]\d*\)/)
  await sessionsBtn.click()
  const modal = page.getByTestId('card-sessions-modal')
  const implRow = modal.locator('[data-testid="card-run-row"][data-role="work"]').first()
  const reviewRow = modal.locator('[data-testid="card-run-row"][data-role="review"]').first()
  await expect(implRow).toContainText('Implementation')
  await expect(implRow.getByTestId('card-run-summary')).toHaveText(implSummary)
  await expect(implRow).toContainText('Sealed')
  await expect(reviewRow).toContainText('Review')
  await expect(reviewRow.getByTestId('card-run-summary')).toHaveText(reviewSummary)

  // Open the implementation run: sealed, read-only transcript.
  await implRow.click()
  await expect(page).toHaveURL(/\/sessions\//)
  await expect(page.getByTestId('chat-sealed-banner')).toContainText('this run is finished', {
    timeout: 10_000,
  })
  await expect(page.locator('.input-bar')).toHaveCount(0)

  // Its tab chip carries the sealed marker, and the tab menu drops the
  // agent controls (worker session — only the tab payload knows it's sealed).
  const sessionId = new URL(page.url()).pathname.split('/').pop()
  const chip = page.locator(`.tab-wrap[data-tab-id="session:${sessionId}"]`)
  await expect(chip.locator('.tab-icon-sealed-session')).toBeVisible({ timeout: 10_000 })
  await chip.locator('.tab-opened').click({ button: 'right' })
  const menu = page.locator('.context-menu')
  await expect(menu.getByRole('menuitem', { name: 'Rename' })).toBeVisible()
  await expect(menu.getByRole('menuitem', { name: 'Close tab' })).toBeVisible()
  for (const label of ['Auto-switch model', 'Clear session', 'Terminate agent']) {
    await expect(menu.getByRole('menuitem', { name: label })).toHaveCount(0)
  }
})
