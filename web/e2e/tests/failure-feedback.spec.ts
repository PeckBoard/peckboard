import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Failed writes must be visible. Two paths used to fail silently:
 *
 *  - deleting a card: a failing DELETE left the confirm modal sitting there
 *    with no message, and the card still on the board;
 *  - the plan → cards wizard: a failing POST still closed the wizard and
 *    navigated, so the user believed cards were being created.
 *
 * Both now surface the server's message and keep their modal open.
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

async function makeFolder(request: APIRequestContext, auth: Auth['auth'], slug: string) {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-${slug}-`))
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-${slug}-${Date.now()}`, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as { id: string }
}

test('a failing card DELETE shows the error in the confirm modal and keeps the card', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)

  const folder = await makeFolder(request, auth, 'delete-fail')
  // worker_count=0 keeps the card parked in backlog so nothing races.
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: { name: 'delete failure', folder_id: folder.id, worker_count: 0, workflow: 'task' },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: auth,
    data: { title: 'Undeletable Card', description: '', step: 'backlog', priority: 1 },
  })
  expect(cardRes.ok(), `seed card failed: ${await cardRes.text()}`).toBeTruthy()

  // Only the DELETE fails; the cards GET must still go through so the board
  // keeps rendering the card.
  await page.route('**/api/projects/*/cards/*', async (route) => {
    if (route.request().method() !== 'DELETE') return route.fallback()
    await route.fulfill({
      status: 500,
      contentType: 'application/json',
      body: JSON.stringify({ error: 'card delete blew up' }),
    })
  })

  await loadAt(page, token, `/projects/${project.id}`)
  const card = page.locator('.kanban-card').filter({ hasText: 'Undeletable Card' })
  await expect(card).toBeVisible({ timeout: 15_000 })

  await card.locator('.kanban-card-menu-btn').click()
  await page.locator('.kanban-card-menu button', { hasText: /^Delete$/ }).click()

  const confirm = page.locator('.modal', { hasText: 'Delete card?' })
  await expect(confirm).toBeVisible({ timeout: 5_000 })
  const deleteBtn = confirm.locator('.btn-danger')
  await deleteBtn.click()

  const error = page.locator('[data-testid="card-delete-error"]')
  await expect(error).toBeVisible({ timeout: 5_000 })
  await expect(error).toContainText('card delete blew up')
  // Modal stays open, Delete stays clickable for a retry, card survives.
  await expect(confirm).toBeVisible()
  await expect(deleteBtn).toBeEnabled()
  await expect(card).toBeVisible()

  await page.unroute('**/api/projects/*/cards/*')
  const del = await request.delete(`/api/projects/${project.id}`, { headers: auth })
  expect(del.ok(), `delete project failed: ${await del.text()}`).toBeTruthy()
})

test('a failing plan-wizard submit keeps the wizard open with an error', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)

  // The only way to get a real plan is the deterministic `mock:plan-review`
  // worker, which persists one through the same `upsert_plan` path the
  // `propose_plan` MCP tool uses (see plan-flow.spec.ts).
  const folder = await makeFolder(request, auth, 'wizard-fail')
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: 'wizard failure',
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

  const plan = await poll(
    async () => {
      const res = await request.get(`/api/plans?card_id=${card.id}`, { headers: auth })
      if (res.status() === 204 || !res.ok()) return null
      return (await res.json()).plan as { id: string }
    },
    30_000,
    'worker to persist a plan',
  )

  await page.route('**/api/sessions/*/message', async (route) => {
    await route.fulfill({
      status: 500,
      contentType: 'application/json',
      body: JSON.stringify({ error: 'session is busy' }),
    })
  })

  await loadAt(page, token, `/plan/${plan.id}`)
  await expect(page.locator('[data-testid="plan-view"]')).toBeVisible({ timeout: 15_000 })
  await page.locator('[data-testid="plan-create-cards"]').click()

  const wizard = page.locator('[data-testid="plan-wizard"]')
  await expect(wizard).toBeVisible({ timeout: 5_000 })

  const projectSelect = page.locator('#plan-wizard-project')
  await expect(projectSelect.locator('option')).not.toHaveCount(1, { timeout: 10_000 })
  await projectSelect.selectOption(project.id)
  const providerSelect = page.locator('#plan-wizard-provider')
  await expect(providerSelect.locator('option')).not.toHaveCount(1, { timeout: 10_000 })
  await providerSelect.selectOption({ index: 1 })

  await page.locator('[data-testid="plan-wizard-create"]').click()

  const error = page.locator('[data-testid="plan-wizard-error"]')
  await expect(error).toBeVisible({ timeout: 5_000 })
  await expect(error).toContainText('session is busy')
  // `onSent` did not fire: the wizard is still open and we never navigated
  // to the authoring session.
  await expect(wizard).toBeVisible()
  await expect(page).toHaveURL(new RegExp(`/plan/${plan.id}$`))

  await page.unroute('**/api/sessions/*/message')
  const del = await request.delete(`/api/projects/${project.id}`, { headers: auth })
  expect(del.ok(), `delete project failed: ${await del.text()}`).toBeTruthy()
})
