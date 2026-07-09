import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * End-to-end for the plan feature: a worker session works a ticket using the
 * deterministic `mock:plan-review` scenario, which persists a plan (Markdown +
 * a mermaid diagram) through the SAME `upsert_plan` path the `propose_plan`
 * MCP tool uses. We verify the plan is durable, reachable from the card's
 * 3-dots menu, rendered full-page, and that per-line review comments work —
 * then delete the project so the test leaves nothing behind.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type Auth = { token: string; auth: Record<string, string> }

async function authenticate(request: APIRequestContext): Promise<Auth> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

async function poll<T>(fn: () => Promise<T | null>, timeoutMs: number, what: string): Promise<T> {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    const v = await fn()
    if (v) return v
    await new Promise((r) => setTimeout(r, 400))
  }
  throw new Error(`timed out waiting for ${what}`)
}

test('worker persists a plan; it is durable, viewable, and reviewable', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)

  // 1. Project with one worker on the thinking mock model + a card.
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-plan-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-plan-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: 'plan flow',
      folder_id: folder.id,
      worker_count: 1,
      workflow: 'task',
      model: 'mock:plan-review',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: auth,
    data: { title: 'Build the widget', description: 'do it', step: 'backlog', priority: 1 },
  })
  expect(cardRes.ok(), `create card failed: ${await cardRes.text()}`).toBeTruthy()
  const card = (await cardRes.json()) as { id: string }

  // 2. Watch the worker work the ticket: poll until a plan is persisted for
  //    the card (the orchestrator spawns the worker, which saves the plan).
  const plan = await poll(
    async () => {
      const res = await request.get(`/api/plans?card_id=${card.id}`, { headers: auth })
      if (res.status() === 204 || !res.ok()) return null
      return (await res.json()).plan as { id: string; markdown: string; title: string }
    },
    30_000,
    'worker to persist a plan',
  )
  expect(plan.title).toBe('Widget plan')
  expect(plan.markdown).toContain('```mermaid')
  expect(plan.markdown).toContain('Implement the widget')

  // 3. Durability: the plan is a row, not an event, so it is still there
  //    after the worker's turn ended (and would survive a session clear).
  const again = await request.get(`/api/plans/${plan.id}`, { headers: auth })
  expect(again.ok()).toBeTruthy()

  // 4. UI — the card's 3-dots menu exposes an enabled "Plan" item that opens
  //    the full-page rendered view.
  await loadAt(page, token, `/projects/${project.id}`)
  const cardEl = page.locator('.kanban-card').filter({ hasText: 'Build the widget' })
  await expect(cardEl).toBeVisible({ timeout: 15_000 })
  await cardEl.locator('.kanban-card-menu-btn').click()
  const planItem = page.locator('[data-testid="card-menu-plan"]')
  await expect(planItem).toBeVisible({ timeout: 5_000 })
  await expect(planItem).toBeEnabled({ timeout: 5_000 })
  await planItem.click()

  await expect(page).toHaveURL(new RegExp(`/plan/${plan.id}$`))
  await expect(page.locator('[data-testid="plan-view"]')).toBeVisible({ timeout: 10_000 })
  await expect(page.locator('[data-testid="plan-title"]')).toHaveText('Widget plan')
  await expect(page.locator('[data-testid="plan-rendered"]')).toContainText('Implement the widget')

  // 5. Per-line review: switch to review mode, comment on a line, see it land.
  await page.locator('[data-testid="plan-tab-review"]').click()
  const addBtn = page.locator('[data-testid^="plan-comment-add-"]').first()
  await addBtn.click()
  await page.locator('[data-testid="plan-comment-input"]').fill('tighten step 1')
  await page.locator('[data-testid="plan-comment-save"]').click()
  await expect(page.locator('[data-testid="plan-comment"]')).toContainText('tighten step 1')

  // The comment is durable via the API too.
  const comments = await request.get(`/api/plans/${plan.id}/comments`, { headers: auth })
  expect(((await comments.json()).comments as unknown[]).length).toBeGreaterThan(0)
  // 6. Delete the plan from its own button (with confirm) and verify it's gone.
  await page.locator('[data-testid="plan-tab-read"]').click()
  await page.locator('[data-testid="plan-delete"]').click()
  await page.locator('.confirm-dialog-danger').click()
  await expect
    .poll(async () => (await request.get(`/api/plans/${plan.id}`, { headers: auth })).status())
    .toBe(404)

  // 6. Clean up — delete the project (cascades cards, sessions, events). The
  //    plan rows go with it.
  const del = await request.delete(`/api/projects/${project.id}`, { headers: auth })
  expect(del.ok(), `delete project failed: ${await del.text()}`).toBeTruthy()
  const gone = await request.get(`/api/plans/${plan.id}`, { headers: auth })
  expect(gone.status()).toBe(404)
})
