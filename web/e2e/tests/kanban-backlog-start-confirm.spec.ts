import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Moving a card out of Backlog is the one board gesture that can't be
 * undone: it spawns a paid worker immediately and permanently freezes the
 * card's description and workflow (enforced server-side in
 * `src/routes/projects/cards.rs`). The board asks first.
 *
 * Covered here:
 *   1. the first drop out of Backlog raises the dialog; cancelling leaves
 *      the card in Backlog, confirming moves it;
 *   2. the "Move to" menu — the only step-change path on touch/keyboard —
 *      is gated by the same dialog, not a second copy of it;
 *   3. ticking "don't ask again" suppresses the dialog on later moves.
 *
 * Projects use `worker_count: 0` so the orchestrator can't move a card
 * itself and race the assertions.
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

/** Loads the app authenticated, with the confirmation left ENABLED (the
 *  default for a fresh install — no skip flag seeded). */
async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

/** Parked project with two backlog cards. Returns the project id. */
async function seedProject(
  request: APIRequestContext,
  auth: Record<string, string>,
  slug: string,
): Promise<string> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-${slug}-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-${slug}-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: `backlog confirm ${slug}`,
      folder_id: folder.id,
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  for (const [i, title] of ['Alpha', 'Beta'].entries()) {
    const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
      headers: auth,
      data: { title, description: '', step: 'backlog', priority: i },
    })
    expect(cardRes.ok(), `seed card ${title} failed: ${await cardRes.text()}`).toBeTruthy()
  }
  return project.id
}

function columnNamed(page: Page, label: string) {
  return page
    .locator('.kanban-column')
    .filter({ has: page.locator('h3', { hasText: new RegExp(`^${label}$`) }) })
}

async function stepOf(
  request: APIRequestContext,
  auth: Record<string, string>,
  projectId: string,
  title: string,
): Promise<string | undefined> {
  const res = await request.get(`/api/projects/${projectId}/cards`, { headers: auth })
  expect(res.ok(), `list cards failed: ${await res.text()}`).toBeTruthy()
  const cards = (await res.json()) as { title: string; step: string }[]
  return cards.find((c) => c.title === title)?.step
}

test('a drop out of Backlog is confirmed first, and cancelling leaves the card put', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const projectId = await seedProject(request, auth, 'drop')
  await loadAt(page, token, `/projects/${projectId}`)

  const backlog = columnNamed(page, 'Backlog')
  const inProgress = columnNamed(page, 'In Progress')
  const alpha = backlog.locator('.kanban-card', { hasText: 'Alpha' })
  await expect(alpha).toBeVisible({ timeout: 10_000 })

  const dialog = page.locator('[data-testid="backlog-start-confirm"]')

  // Drop → dialog, and the card has NOT moved behind it.
  await alpha.dragTo(inProgress)
  await expect(dialog).toBeVisible()
  await expect(dialog).toContainText('costs money')
  await expect(dialog).toContainText('lock')
  await expect(backlog.locator('.kanban-card', { hasText: 'Alpha' })).toBeVisible()

  // Cancel → still in Backlog, in the UI and on the server.
  await dialog.getByRole('button', { name: 'Keep in Backlog' }).click()
  await expect(dialog).toHaveCount(0)
  await expect(backlog.locator('.kanban-card', { hasText: 'Alpha' })).toBeVisible()
  await expect(inProgress.locator('.kanban-card', { hasText: 'Alpha' })).toHaveCount(0)
  expect(await stepOf(request, auth, projectId, 'Alpha')).toBe('backlog')

  // Drop again and confirm → the move goes through.
  await backlog.locator('.kanban-card', { hasText: 'Alpha' }).dragTo(inProgress)
  await expect(dialog).toBeVisible()
  await dialog.getByRole('button', { name: 'Start work' }).click()
  await expect(dialog).toHaveCount(0)
  await expect(inProgress.locator('.kanban-card', { hasText: 'Alpha' })).toBeVisible({
    timeout: 10_000,
  })
  await expect.poll(() => stepOf(request, auth, projectId, 'Alpha')).toBe('in_progress')
})

test('the "Move to" menu raises the same confirmation', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const projectId = await seedProject(request, auth, 'menu')
  await loadAt(page, token, `/projects/${projectId}`)

  const backlog = columnNamed(page, 'Backlog')
  const alpha = backlog.locator('.kanban-card', { hasText: 'Alpha' })
  await expect(alpha).toBeVisible({ timeout: 10_000 })

  await alpha.locator('.kanban-card-menu-btn').click()
  await page.locator('[data-testid="card-menu-move"]').click()
  await page.locator('[data-testid="card-menu-move-in_progress"]').click()

  const dialog = page.locator('[data-testid="backlog-start-confirm"]')
  await expect(dialog).toBeVisible()
  expect(await stepOf(request, auth, projectId, 'Alpha')).toBe('backlog')

  await dialog.getByRole('button', { name: 'Start work' }).click()
  await expect(
    columnNamed(page, 'In Progress').locator('.kanban-card', { hasText: 'Alpha' }),
  ).toBeVisible({ timeout: 10_000 })
  await expect.poll(() => stepOf(request, auth, projectId, 'Alpha')).toBe('in_progress')
})

test('"don\'t ask again" suppresses the confirmation for later moves', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const projectId = await seedProject(request, auth, 'skip')
  await loadAt(page, token, `/projects/${projectId}`)

  const backlog = columnNamed(page, 'Backlog')
  const inProgress = columnNamed(page, 'In Progress')
  const dialog = page.locator('[data-testid="backlog-start-confirm"]')

  const alpha = backlog.locator('.kanban-card', { hasText: 'Alpha' })
  await expect(alpha).toBeVisible({ timeout: 10_000 })
  await alpha.dragTo(inProgress)
  await expect(dialog).toBeVisible()
  await dialog.locator('[data-testid="confirm-dialog-checkbox"]').check()
  await dialog.getByRole('button', { name: 'Start work' }).click()
  await expect(inProgress.locator('.kanban-card', { hasText: 'Alpha' })).toBeVisible({
    timeout: 10_000,
  })

  // Second move: straight through, no dialog. The preference is persisted
  // per install, so it also survives a reload.
  await page.reload()
  const beta = columnNamed(page, 'Backlog').locator('.kanban-card', { hasText: 'Beta' })
  await expect(beta).toBeVisible({ timeout: 10_000 })
  await beta.dragTo(columnNamed(page, 'In Progress'))
  await expect(
    columnNamed(page, 'In Progress').locator('.kanban-card', { hasText: 'Beta' }),
  ).toBeVisible({ timeout: 10_000 })
  await expect(dialog).toHaveCount(0)
  await expect.poll(() => stepOf(request, auth, projectId, 'Beta')).toBe('in_progress')
})
