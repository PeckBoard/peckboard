import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Drag-and-drop on the kanban board at the default (desktop) viewport,
 * where the board renders as classic vertical-columns kanban: steps
 * side by side, cards stacked top-to-bottom inside each column.
 *
 * Three user-visible behaviours, one assertion each:
 *
 *   1. **Cross-column** — dragging a card across a column boundary
 *      persists a step transition.
 *   2. **In-column** — dragging a card past a sibling within a column
 *      persists a priority/bucket change (the backend orders by
 *      `priority ASC`). On the desktop vertical-columns layout the
 *      insertion axis is vertical (top half = insert before, bottom
 *      half = insert after); on release, the dragged card's priority
 *      adopts the leading neighbour's so the column order survives a
 *      refresh.
 *   3. **Column geometry** — the board renders step columns
 *      left-to-right in the canonical step order, cards within a
 *      column render top-to-bottom in priority-ASC order, and a
 *      cross-column drop survives a full reload.
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
    data: {
      name: `dnd project ${suffix}`,
      folder_id: folder.id,
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  // Three cards with distinct priorities so backend ASC sort places them
  // in a known order in the backlog row.
  const titles = ['First', 'Second', 'Third']
  const priorities = [0, 2, 3]
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

test('cross-column drag from backlog to in_progress persists the step change', async ({
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

test('in-column drag past a sibling adopts the leading neighbour priority', async ({
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

  // Drop "Third" onto the top half of "First" — on the desktop
  // vertical-columns layout the insertion axis is vertical, so this is
  // an insertIdx of 0, which adopts the trailing-neighbour priority
  // (First's = 0).
  const firstBox = await first.boundingBox()
  expect(firstBox).toBeTruthy()
  await third.dragTo(first, {
    targetPosition: { x: firstBox!.width / 2, y: 4 },
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

test('columns stack horizontally, cards stack vertically, and order survives a reload', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)
  const { projectId, cards } = await setupProjectWithCards(request, auth, 'geom')

  // Park one card in each of the first three steps so we can assert the
  // horizontal ordering of columns independently of the priority order
  // of cards within a column.
  await request.put(`/api/projects/${projectId}/cards/${cards[1].id}`, {
    headers: auth,
    data: { step: 'in_progress' },
  })
  await request.put(`/api/projects/${projectId}/cards/${cards[2].id}`, {
    headers: auth,
    data: { step: 'review' },
  })

  await loadAt(page, token, `/projects/${projectId}`)

  // Match columns by their header heading exactly — priority 4 cards render
  // a "Backlog" badge inside the card, so a `hasText: 'Backlog'` filter on
  // .kanban-column matches multiple columns once "Third" lands in Review.
  const columnByLabel = (label: string) =>
    page.locator('.kanban-column').filter({
      has: page.locator('.kanban-column-header h3', { hasText: new RegExp(`^${label}$`) }),
    })
  const backlogCol = columnByLabel('Backlog')
  const inProgressCol = columnByLabel('In Progress')
  const reviewCol = columnByLabel('Review')
  await expect(backlogCol.locator('.kanban-card-title', { hasText: 'First' })).toBeVisible({
    timeout: 10_000,
  })
  await expect(inProgressCol.locator('.kanban-card-title', { hasText: 'Second' })).toBeVisible()
  await expect(reviewCol.locator('.kanban-card-title', { hasText: 'Third' })).toBeVisible()

  // Columns stack left-to-right: Backlog left of In Progress left of Review.
  const backlogBox = await backlogCol.boundingBox()
  const inProgressBox = await inProgressCol.boundingBox()
  const reviewBox = await reviewCol.boundingBox()
  expect(backlogBox && inProgressBox && reviewBox, 'all three columns measurable').toBeTruthy()
  expect(backlogBox!.x, 'Backlog left of In Progress').toBeLessThan(inProgressBox!.x)
  expect(inProgressBox!.x, 'In Progress left of Review').toBeLessThan(reviewBox!.x)

  // Move "Third" back into the backlog column so we have two siblings to
  // assert vertical ordering on. Priority 4 keeps it below "First"
  // (priority 0).
  await request.put(`/api/projects/${projectId}/cards/${cards[2].id}`, {
    headers: auth,
    data: { step: 'backlog' },
  })

  const first = backlogCol.locator('.kanban-card', { hasText: 'First' })
  const third = backlogCol.locator('.kanban-card', { hasText: 'Third' })
  await expect(third).toBeVisible({ timeout: 5_000 })

  // Cards within a column stack top-to-bottom in priority-ASC order.
  // Compare card-center y's so a tiny overlap from focus rings doesn't
  // flip the comparison.
  const firstBox = await first.boundingBox()
  const thirdBox = await third.boundingBox()
  expect(firstBox && thirdBox, 'both backlog cards measurable').toBeTruthy()
  expect(
    firstBox!.y + firstBox!.height / 2,
    'First (priority 0) above Third (priority 4)',
  ).toBeLessThan(thirdBox!.y + thirdBox!.height / 2)
  // And the column really is laid out vertically — the cards' horizontal
  // extents overlap (a column layout), they're not stacked side-by-side.
  const firstXRange = [firstBox!.x, firstBox!.x + firstBox!.width]
  const thirdXRange = [thirdBox!.x, thirdBox!.x + thirdBox!.width]
  expect(
    Math.min(firstXRange[1], thirdXRange[1]) - Math.max(firstXRange[0], thirdXRange[0]),
    'siblings horizontally overlap (vertical column layout)',
  ).toBeGreaterThan(0)

  // Cross-column drag persists across reload. Drag "First" from backlog
  // into the in-progress column, then reload and confirm it's still in
  // the in-progress column (and gone from backlog).
  await first.dragTo(inProgressCol)
  await expect(inProgressCol.locator('.kanban-card-title', { hasText: 'First' })).toBeVisible({
    timeout: 5_000,
  })

  await page.reload()
  const reloadedInProgress = columnByLabel('In Progress')
  const reloadedBacklog = columnByLabel('Backlog')
  await expect(reloadedInProgress.locator('.kanban-card-title', { hasText: 'First' })).toBeVisible({
    timeout: 10_000,
  })
  await expect(reloadedBacklog.locator('.kanban-card-title', { hasText: 'First' })).toHaveCount(0)
})
