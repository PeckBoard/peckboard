import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Drag-and-drop on the horizontal kanban board.
 *
 * Two user-visible behaviours, one assertion each:
 *
 *   1. **Cross-row** — dragging a card vertically from one row into another
 *      persists a step transition. The destination row shows the accept
 *      band while the drag is over it, and the card lands in the new row
 *      after the drop.
 *   2. **In-row** — dragging a card horizontally past a sibling persists a
 *      priority/bucket change (the backend orders by `priority ASC`). The
 *      drop indicator appears as a vertical line between cards; on release,
 *      the dragged card's priority adopts the leading neighbour's so the
 *      row order is preserved across a refresh.
 *   3. **Horizontal geometry** — the board renders step rows top-to-bottom
 *      in the canonical step order, cards within a row render left-to-right
 *      in priority-ASC order, and a cross-row drop survives a full reload.
 *
 * Project is created with `worker_count: 0` so the orchestrator never
 * picks up our cards and changes their `step` / `worker_session_id`
 * mid-test.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type Card = { id: string; title: string; step: string; priority: number }

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

async function getCard(
  request: APIRequestContext,
  auth: Record<string, string>,
  projectId: string,
  cardId: string,
): Promise<Card> {
  const res = await request.get(`/api/projects/${projectId}/cards`, { headers: auth })
  expect(res.ok(), `list cards failed: ${await res.text()}`).toBeTruthy()
  const list = (await res.json()) as Card[]
  const card = list.find((c) => c.id === cardId)
  expect(card, `card ${cardId} present after drop`).toBeTruthy()
  return card!
}

async function setupProjectWithCards(
  request: APIRequestContext,
  auth: Record<string, string>,
  suffix: string,
): Promise<{ projectId: string; cards: Card[] }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-dnd-${suffix}-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-dnd-${suffix}-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: { name: `dnd project ${suffix}`, folder_id: folder.id, worker_count: 0 },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  // Three cards with distinct priorities so backend ASC sort places them
  // in a known order in the backlog row.
  const titles = ['First', 'Second', 'Third']
  const priorities = [0, 2, 4]
  const cards: Card[] = []
  for (let i = 0; i < titles.length; i++) {
    const res = await request.post(`/api/projects/${project.id}/cards`, {
      headers: auth,
      data: { title: titles[i], description: '', step: 'backlog', priority: priorities[i] },
    })
    expect(res.ok(), `create card ${titles[i]} failed: ${await res.text()}`).toBeTruthy()
    cards.push((await res.json()) as Card)
  }
  return { projectId: project.id, cards }
}

test('cross-row drag from backlog to in_progress persists the step change', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)
  const { projectId, cards } = await setupProjectWithCards(request, auth, 'cross')

  await loadAt(page, token, `/projects/${projectId}`)

  // All three cards land in the backlog row first.
  const backlogRow = page.locator('.kanban-column', { hasText: 'Backlog' })
  const inProgressRow = page.locator('.kanban-column', { hasText: 'In Progress' })
  await expect(backlogRow.locator('.kanban-card-title', { hasText: 'First' })).toBeVisible({
    timeout: 10_000,
  })

  const source = backlogRow.locator('.kanban-card', { hasText: 'Second' })
  await source.dragTo(inProgressRow)

  // The card moves to the in-progress row in the UI and the API agrees.
  await expect(inProgressRow.locator('.kanban-card-title', { hasText: 'Second' })).toBeVisible({
    timeout: 5_000,
  })
  const moved = await getCard(request, auth, projectId, cards[1].id)
  expect(moved.step).toBe('in_progress')
})

test('in-row drag past a sibling adopts the leading neighbour priority', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)
  const { projectId, cards } = await setupProjectWithCards(request, auth, 'inrow')

  await loadAt(page, token, `/projects/${projectId}`)

  const backlogRow = page.locator('.kanban-column', { hasText: 'Backlog' })
  const first = backlogRow.locator('.kanban-card', { hasText: 'First' })
  const third = backlogRow.locator('.kanban-card', { hasText: 'Third' })
  await expect(first).toBeVisible({ timeout: 10_000 })
  await expect(third).toBeVisible()

  // Drop "Third" onto the left half of "First" — that's an insertIdx of 0,
  // which adopts the trailing-neighbour priority (First's = 0).
  const firstBox = await first.boundingBox()
  expect(firstBox).toBeTruthy()
  await third.dragTo(first, {
    targetPosition: { x: 4, y: firstBox!.height / 2 },
  })

  // Wait for the priority write to round-trip.
  await page.waitForFunction(
    async (cardId) => {
      const res = await fetch(`/api/projects/${location.pathname.split('/').pop()}/cards`, {
        headers: {
          Authorization: `Bearer ${localStorage.getItem('peckboard_token')}`,
        },
      })
      if (!res.ok) return false
      const list = (await res.json()) as { id: string; priority: number }[]
      const c = list.find((x) => x.id === cardId)
      return !!c && c.priority === 0
    },
    cards[2].id,
    { timeout: 5_000 },
  )

  const reordered = await getCard(request, auth, projectId, cards[2].id)
  expect(reordered.priority, 'Third inherits First’s priority bucket').toBe(0)
  expect(reordered.step, 'Third stays in backlog').toBe('backlog')
})

test('rows stack vertically, cards stack horizontally, and order survives a reload', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)
  const { projectId, cards } = await setupProjectWithCards(request, auth, 'geom')

  // Park one card in each of the first three steps so we can assert the
  // vertical stacking order of rows independently of the priority order
  // of cards within a row.
  await request.put(`/api/projects/${projectId}/cards/${cards[1].id}`, {
    headers: auth,
    data: { step: 'in_progress' },
  })
  await request.put(`/api/projects/${projectId}/cards/${cards[2].id}`, {
    headers: auth,
    data: { step: 'review' },
  })

  await loadAt(page, token, `/projects/${projectId}`)

  // Match rows by their header heading exactly — priority 4 cards render a
  // "Backlog" badge inside the card, so a `hasText: 'Backlog'` filter on
  // .kanban-column matches multiple rows once "Third" lands in Review.
  const rowByLabel = (label: string) =>
    page.locator('.kanban-column').filter({
      has: page.locator('.kanban-column-header h3', { hasText: new RegExp(`^${label}$`) }),
    })
  const backlogRow = rowByLabel('Backlog')
  const inProgressRow = rowByLabel('In Progress')
  const reviewRow = rowByLabel('Review')
  await expect(backlogRow.locator('.kanban-card-title', { hasText: 'First' })).toBeVisible({
    timeout: 10_000,
  })
  await expect(inProgressRow.locator('.kanban-card-title', { hasText: 'Second' })).toBeVisible()
  await expect(reviewRow.locator('.kanban-card-title', { hasText: 'Third' })).toBeVisible()

  // Rows stack top-to-bottom: Backlog above In Progress above Review.
  const backlogBox = await backlogRow.boundingBox()
  const inProgressBox = await inProgressRow.boundingBox()
  const reviewBox = await reviewRow.boundingBox()
  expect(backlogBox && inProgressBox && reviewBox, 'all three rows measurable').toBeTruthy()
  expect(backlogBox!.y, 'Backlog above In Progress').toBeLessThan(inProgressBox!.y)
  expect(inProgressBox!.y, 'In Progress above Review').toBeLessThan(reviewBox!.y)

  // Move "Third" back into the backlog row so we have two siblings to
  // assert horizontal ordering on. Priority 4 keeps it to the right of
  // "First" (priority 0).
  await request.put(`/api/projects/${projectId}/cards/${cards[2].id}`, {
    headers: auth,
    data: { step: 'backlog' },
  })

  const first = backlogRow.locator('.kanban-card', { hasText: 'First' })
  const third = backlogRow.locator('.kanban-card', { hasText: 'Third' })
  await expect(third).toBeVisible({ timeout: 5_000 })

  // Cards within a row stack left-to-right in priority-ASC order.
  // Compare card-center x's so a tiny overlap from focus rings doesn't
  // flip the comparison.
  const firstBox = await first.boundingBox()
  const thirdBox = await third.boundingBox()
  expect(firstBox && thirdBox, 'both backlog cards measurable').toBeTruthy()
  expect(
    firstBox!.x + firstBox!.width / 2,
    'First (priority 0) left of Third (priority 4)',
  ).toBeLessThan(thirdBox!.x + thirdBox!.width / 2)
  // And the row really is laid out horizontally — the cards' vertical
  // extents overlap (a row layout), they're not stacked vertically.
  const firstYRange = [firstBox!.y, firstBox!.y + firstBox!.height]
  const thirdYRange = [thirdBox!.y, thirdBox!.y + thirdBox!.height]
  expect(
    Math.min(firstYRange[1], thirdYRange[1]) - Math.max(firstYRange[0], thirdYRange[0]),
    'siblings vertically overlap (horizontal row layout)',
  ).toBeGreaterThan(0)

  // Cross-row drag persists across reload. Drag "First" from backlog into
  // the in-progress row, then reload and confirm it's still in the
  // in-progress row (and gone from backlog).
  await first.dragTo(inProgressRow)
  await expect(inProgressRow.locator('.kanban-card-title', { hasText: 'First' })).toBeVisible({
    timeout: 5_000,
  })

  await page.reload()
  const reloadedInProgress = rowByLabel('In Progress')
  const reloadedBacklog = rowByLabel('Backlog')
  await expect(reloadedInProgress.locator('.kanban-card-title', { hasText: 'First' })).toBeVisible({
    timeout: 10_000,
  })
  await expect(reloadedBacklog.locator('.kanban-card-title', { hasText: 'First' })).toHaveCount(0)
})
