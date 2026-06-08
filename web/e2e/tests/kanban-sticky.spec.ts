import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Card wrapping inside a kanban row.
 *
 * Two user-visible behaviours, one assertion each:
 *
 *   1. **Wrap, don't overflow** — when a row has more cards than fit
 *      across the viewport, the cards wrap onto additional lines inside
 *      the row. The board itself never scrolls horizontally; cards
 *      visually stack on more than one line.
 *   2. **Empty state** — a row with no cards shows a "No cards in …"
 *      placeholder so the row doesn't collapse to a thin strip.
 *
 * Project is created with `worker_count: 0` so the orchestrator never
 * runs anything against our cards.
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

async function setupProject(
  request: APIRequestContext,
  auth: Record<string, string>,
  suffix: string,
  cardCount: number,
): Promise<{ projectId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-wrap-${suffix}-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-wrap-${suffix}-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: { name: `wrap project ${suffix}`, folder_id: folder.id, worker_count: 0 },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  for (let i = 0; i < cardCount; i++) {
    const res = await request.post(`/api/projects/${project.id}/cards`, {
      headers: auth,
      data: { title: `Card ${i + 1}`, description: '', step: 'backlog', priority: 2 },
    })
    expect(res.ok(), `create card ${i} failed: ${await res.text()}`).toBeTruthy()
  }
  return { projectId: project.id }
}

test('cards wrap onto multiple lines without horizontal page scroll', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  // Narrow viewport guarantees the row needs to wrap (cards are 320px;
  // 8 cards = 2560px of card content, far wider than the viewport).
  await page.setViewportSize({ width: 800, height: 700 })

  const { token, auth } = await authenticate(request)
  const { projectId } = await setupProject(request, auth, 'wrap', 8)

  await loadAt(page, token, `/projects/${projectId}`)

  const backlogRow = page.locator('.kanban-column', { hasText: 'Backlog' })
  await expect(backlogRow.locator('.kanban-column-header')).toBeVisible({ timeout: 10_000 })

  // Wait for every card's mount-grow animation to settle so width/y
  // measurements reflect the final laid-out positions.
  await backlogRow
    .locator('.kanban-card')
    .first()
    .evaluate(async (el) => {
      const anims = el.getAnimations({ subtree: false })
      await Promise.all(anims.map((a) => a.finished.catch(() => undefined)))
    })

  // The document never grows wider than the viewport — no horizontal
  // scrollbar on the page.
  const hScroll = await page.evaluate(() => ({
    scrollWidth: document.documentElement.scrollWidth,
    clientWidth: document.documentElement.clientWidth,
  }))
  expect(hScroll.scrollWidth, 'document fits inside viewport width').toBeLessThanOrEqual(
    hScroll.clientWidth + 1,
  )

  // Cards visibly wrap: collect each card's top-y and confirm at least
  // two distinct rows of cards appear inside the backlog row.
  const cards = backlogRow.locator('.kanban-card')
  const count = await cards.count()
  expect(count, 'all 8 cards rendered').toBe(8)
  const tops: number[] = []
  for (let i = 0; i < count; i++) {
    const box = await cards.nth(i).boundingBox()
    expect(box, `card ${i} measurable`).toBeTruthy()
    tops.push(Math.round(box!.y))
  }
  const distinctRows = new Set(tops).size
  expect(distinctRows, 'cards wrap onto at least two visual lines').toBeGreaterThanOrEqual(2)
})

test('empty rows render a placeholder so they do not collapse', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)
  // One card in backlog → every other row is empty.
  const { projectId } = await setupProject(request, auth, 'empty', 1)

  await loadAt(page, token, `/projects/${projectId}`)

  const reviewRow = page.locator('.kanban-column', { hasText: 'Review' })
  await expect(reviewRow.locator('.kanban-cards-empty')).toHaveText(/No cards in Review/, {
    timeout: 10_000,
  })
})
