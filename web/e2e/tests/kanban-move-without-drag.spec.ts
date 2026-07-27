import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * A card must be movable between steps without HTML5 drag-and-drop.
 *
 * DnD never fires on touch and is unreachable from the keyboard, so on a
 * phone or with no pointer at all a card could never leave Backlog. The
 * card "..." menu now carries a "Move to" submenu that drives the same
 * `updateCard(projectId, id, { step })` call as the drop handler.
 *
 * Both tests use `worker_count: 0` projects so the orchestrator can't
 * race the assertions by picking the card up and advancing it itself.
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
    // These tests are about reaching the step-change without a pointer;
    // the backlog confirmation is covered by
    // `kanban-backlog-start-confirm.spec.ts`, so opt out of it here.
    localStorage.setItem('peckboard_skip_backlog_confirm', '1')
  }, token)
  await page.goto(route)
}

/** Fresh folder + parked project + one backlog card. Returns the project id. */
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
      name: `move without drag ${slug}`,
      folder_id: folder.id,
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: auth,
    data: { title: 'Movable Card', description: '', step: 'backlog', priority: 2 },
  })
  expect(cardRes.ok(), `seed card failed: ${await cardRes.text()}`).toBeTruthy()
  return project.id
}

function columnNamed(page: Page, label: string) {
  return page
    .locator('.kanban-column')
    .filter({ has: page.locator('h3', { hasText: new RegExp(`^${label}$`) }) })
}

test.describe('touch only', () => {
  // iPhone-sized viewport with touch emulation: `tap()` dispatches touch
  // events, which never trigger HTML5 dragstart.
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true })

  test('moves a card out of backlog with taps alone', async ({ request, page }) => {
    const { token, auth } = await authenticate(request)
    const projectId = await seedProject(request, auth, 'touch')
    await loadAt(page, token, `/projects/${projectId}`)

    const card = columnNamed(page, 'Backlog').locator('.kanban-card', { hasText: 'Movable Card' })
    await expect(card).toBeVisible({ timeout: 10_000 })

    await card.locator('.kanban-card-menu-btn').tap()
    await page.locator('[data-testid="card-menu-move"]').tap()
    await page.locator('[data-testid="card-menu-move-in_progress"]').tap()

    await expect(
      columnNamed(page, 'In Progress').locator('.kanban-card', { hasText: 'Movable Card' }),
    ).toBeVisible({ timeout: 10_000 })
    await expect(
      columnNamed(page, 'Backlog').locator('.kanban-card', { hasText: 'Movable Card' }),
    ).toHaveCount(0)

    const after = await request.get(`/api/projects/${projectId}/cards`, { headers: auth })
    const cards = (await after.json()) as { title: string; step: string }[]
    expect(cards.find((c) => c.title === 'Movable Card')?.step).toBe('in_progress')
  })
})

test.describe('keyboard only', () => {
  test('moves a card out of backlog with no pointer events', async ({ request, page }) => {
    const { token, auth } = await authenticate(request)
    const projectId = await seedProject(request, auth, 'keyboard')
    await loadAt(page, token, `/projects/${projectId}`)

    const card = columnNamed(page, 'Backlog').locator('.kanban-card', { hasText: 'Movable Card' })
    await expect(card).toBeVisible({ timeout: 10_000 })

    // The "..." trigger is a native <button>, so it is tab-reachable;
    // focus() just skips the tab walk. Everything after this is keys only.
    const trigger = card.locator('.kanban-card-menu-btn')
    await trigger.focus()
    await expect(trigger).toBeFocused()
    await page.keyboard.press('Enter')

    // Opening the menu moves focus into it; ArrowDown walks the enabled
    // rows (the disabled "Plan" row is skipped).
    const moveRow = page.locator('[data-testid="card-menu-move"]')
    await expect(moveRow).toBeVisible()
    for (let i = 0; i < 8; i++) {
      if (await moveRow.evaluate((el) => el === document.activeElement)) break
      await page.keyboard.press('ArrowDown')
    }
    await expect(moveRow).toBeFocused()

    // ArrowRight opens the submenu and lands on its first enabled row —
    // Backlog is the current step, so it is disabled and skipped.
    await page.keyboard.press('ArrowRight')
    await expect(page.locator('[data-testid="card-menu-move-in_progress"]')).toBeFocused()
    await page.keyboard.press('Enter')

    await expect(
      columnNamed(page, 'In Progress').locator('.kanban-card', { hasText: 'Movable Card' }),
    ).toBeVisible({ timeout: 10_000 })

    const after = await request.get(`/api/projects/${projectId}/cards`, { headers: auth })
    const cards = (await after.json()) as { title: string; step: string }[]
    expect(cards.find((c) => c.title === 'Movable Card')?.step).toBe('in_progress')
  })
})
