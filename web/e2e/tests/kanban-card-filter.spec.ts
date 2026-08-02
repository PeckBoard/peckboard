import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Kanban board card filter box: client-side title/description match
 * that hides non-matching cards and updates each column's match count.
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

async function seedProject(
  request: APIRequestContext,
  auth: Record<string, string>,
): Promise<{ projectId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-kanban-filter-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-kanban-filter-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: { name: 'kanban filter e2e', folder_id: folder.id, worker_count: 0, workflow: 'task' },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }
  return { projectId: project.id }
}

test('kanban board filter hides non-matching cards and updates column counts', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const { projectId } = await seedProject(request, auth)

  const cards: [string, string, string][] = [
    ['Fix login bug', 'users cannot sign in', 'backlog'],
    ['Add dark mode', 'theme toggle in settings', 'backlog'],
    ['Refactor auth module', 'unrelated to login title but matches description', 'in_progress'],
  ]
  for (const [title, description, step] of cards) {
    const res = await request.post(`/api/projects/${projectId}/cards`, {
      headers: auth,
      data: { title, description, step, priority: 2 },
    })
    expect(res.ok(), `seed ${title} failed: ${await res.text()}`).toBeTruthy()
  }

  await loadAt(page, token, `/projects/${projectId}`)
  await expect(page.getByTestId('kanban-filter')).toBeVisible({ timeout: 10_000 })

  const backlog = page.locator('.kanban-column', { hasText: 'Backlog' })
  await expect(backlog.locator('.kanban-card')).toHaveCount(2, { timeout: 10_000 })

  // Title match: "login" only hits the first backlog card.
  await page.getByTestId('kanban-filter').fill('login')
  await expect(backlog.locator('.kanban-card')).toHaveCount(1)
  await expect(backlog.locator('.kanban-card-title', { hasText: 'Fix login bug' })).toBeVisible()
  await expect(backlog.locator('.kanban-count')).toHaveText('1')

  // Description match: "unrelated" only hits the in_progress card,
  // which doesn't match the title at all.
  await page.getByTestId('kanban-filter').fill('unrelated')
  await expect(backlog.locator('.kanban-card')).toHaveCount(0)
  await expect(backlog.locator('.kanban-cards-empty')).toHaveText('No matching cards')
  const inProgress = page.locator('.kanban-column', { hasText: 'In Progress' })
  await expect(inProgress.locator('.kanban-card')).toHaveCount(1)

  // Clearing restores every card.
  await page.getByTestId('kanban-filter').fill('')
  await expect(backlog.locator('.kanban-card')).toHaveCount(2)
})
