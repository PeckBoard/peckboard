import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Backlog cards must not surface worker actions. The orchestrator
 * auto-spawns when capacity is free, so "Restart Worker" reads as a
 * confusing no-op offer. View Session / Stop Worker / the worker dot
 * are likewise irrelevant for a card that hasn't been picked up.
 *
 * The card menu still shows Edit / Details / Delete on backlog. This
 * test pins the boundary so a future refactor that re-introduces the
 * worker affordances will fail loudly.
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
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

test('backlog card menu omits Restart/Stop/View Session', async ({ request, page, baseURL }) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-backlog-aff-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-backlog-aff-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    // worker_count=0 keeps the card parked in backlog so the menu
    // assertions don't race with an orchestrator spawn.
    data: {
      name: `backlog affordances`,
      folder_id: folder.id,
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: auth,
    data: { title: 'Backlog Card', description: '', step: 'backlog', priority: 2 },
  })
  expect(cardRes.ok(), `seed card failed: ${await cardRes.text()}`).toBeTruthy()

  await loadAt(page, token, `/projects/${project.id}`)

  const backlog = page.locator('.kanban-column', { hasText: 'Backlog' })
  const card = backlog.locator('.kanban-card').filter({ hasText: 'Backlog Card' })
  await expect(card).toBeVisible({ timeout: 10_000 })

  // Open the per-card menu via the "..." trigger and assert the
  // entries: only Edit / Details / Cancel as Won't Do / Delete should
  // appear. Restart Worker, Stop Worker, and View Session must not.
  await card.locator('.kanban-card-menu-btn').click()
  const menu = page.locator('.kanban-card-menu')
  await expect(menu).toBeVisible({ timeout: 5_000 })
  await expect(menu.locator('button', { hasText: /^Edit$/ })).toBeVisible()
  await expect(menu.locator('button', { hasText: /^Details$/ })).toBeVisible()
  await expect(menu.locator('button', { hasText: /Restart Worker/ })).toHaveCount(0)
  await expect(menu.locator('button', { hasText: /Stop Worker/ })).toHaveCount(0)
  await expect(menu.locator('button', { hasText: /View Session/ })).toHaveCount(0)
})
