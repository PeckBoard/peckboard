import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Phone kanban layout + touch affordances.
 *
 * Below the 768px breakpoint the board drops the side-by-side columns
 * (which forced a horizontal scroll at ~1.6 visible columns) for a
 * vertical stack of full-width step sections, and — because HTML5 drag
 * never fires on touch — every card grows an always-visible move button
 * (coarse pointer only) that opens a bottom move sheet.
 *
 * All tests use `worker_count: 0` projects so the orchestrator can't race
 * the assertions by picking cards up itself.
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
    // The backlog "start work" confirmation is covered by
    // `kanban-backlog-start-confirm.spec.ts`; opt out of it here so the
    // move-sheet flow is the thing under test.
    localStorage.setItem('peckboard_skip_backlog_confirm', '1')
  }, token)
  await page.goto(route)
}

/** Fresh folder + parked project + two backlog cards. */
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
      name: `kanban mobile ${slug}`,
      folder_id: folder.id,
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  for (const title of ['Phone Card A', 'Phone Card B']) {
    const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
      headers: auth,
      data: { title, description: 'a mobile-layout card', step: 'backlog', priority: 2 },
    })
    expect(cardRes.ok(), `seed ${title} failed: ${await cardRes.text()}`).toBeTruthy()
  }
  return project.id
}

function columnNamed(page: Page, label: string) {
  return page
    .locator('.kanban-column')
    .filter({ has: page.locator('h3', { hasText: new RegExp(`^${label}$`) }) })
}

// iPhone-sized viewport with touch emulation: `(pointer: coarse)` matches,
// `tap()` dispatches touch events.
test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true })

test('phone board stacks step sections vertically without horizontal scroll', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const projectId = await seedProject(request, auth, 'kanban-stack')
  await loadAt(page, token, `/projects/${projectId}`)

  const backlog = columnNamed(page, 'Backlog')
  await expect(backlog.locator('.kanban-card')).toHaveCount(2, { timeout: 10_000 })

  // The board scroller must not overflow horizontally on a phone.
  const overflow = await page.evaluate(() => {
    const el = document.querySelector('.kanban-board-scroll')!
    return el.scrollWidth - el.clientWidth
  })
  expect(overflow, 'no horizontal overflow on the board').toBeLessThanOrEqual(1)

  // Step sections stack top-to-bottom, each spanning the full board width.
  const backlogBox = (await backlog.boundingBox())!
  const inProgressBox = (await columnNamed(page, 'In Progress').boundingBox())!
  expect(inProgressBox.y).toBeGreaterThan(backlogBox.y + backlogBox.height - 1)
  expect(backlogBox.width).toBeGreaterThan(300)
})

test('touch move button opens the bottom sheet and moves the card', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const projectId = await seedProject(request, auth, 'kanban-sheet')
  await loadAt(page, token, `/projects/${projectId}`)

  const card = columnNamed(page, 'Backlog').locator('.kanban-card', { hasText: 'Phone Card A' })
  await expect(card).toBeVisible({ timeout: 10_000 })

  // Coarse pointer ⇒ the move affordance is always visible on the card.
  const moveBtn = card.getByTestId('card-move-btn')
  await expect(moveBtn).toBeVisible()
  await moveBtn.tap()

  const sheet = page.getByTestId('card-move-sheet')
  await expect(sheet).toBeVisible()
  await expect(sheet.getByText('Move “Phone Card A”')).toBeVisible()
  // The current step is listed but not tappable.
  await expect(sheet.getByTestId('card-move-step-backlog')).toBeDisabled()

  await sheet.getByTestId('card-move-step-in_progress').tap()

  await expect(sheet).toHaveCount(0)
  await expect(
    columnNamed(page, 'In Progress').locator('.kanban-card', { hasText: 'Phone Card A' }),
  ).toBeVisible({ timeout: 10_000 })
})

test('tapping the card title opens the detail modal directly', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const projectId = await seedProject(request, auth, 'kanban-title')
  await loadAt(page, token, `/projects/${projectId}`)

  const card = columnNamed(page, 'Backlog').locator('.kanban-card', { hasText: 'Phone Card B' })
  await expect(card).toBeVisible({ timeout: 10_000 })

  await card.locator('.kanban-card-title').tap()

  const modal = page.locator('.modal')
  await expect(modal.locator('h2', { hasText: 'Phone Card B' })).toBeVisible({ timeout: 5_000 })
  await expect(modal.locator('.card-detail-description')).toContainText('a mobile-layout card')
})
