import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Sticky row headers + per-row horizontal scroll on the kanban board.
 *
 * Two user-visible behaviours, one assertion each:
 *
 *   1. **Sticky header** — when a row has more cards than fit, scrolling
 *      that row horizontally leaves the step label pinned at the row's
 *      left edge. The label's viewport x stays put while cards slide
 *      past it.
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
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-sticky-${suffix}-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-sticky-${suffix}-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: { name: `sticky project ${suffix}`, folder_id: folder.id, worker_count: 0 },
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

test('row header stays pinned at the left while cards scroll horizontally', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  // Narrow viewport guarantees the row overflows and scrolls horizontally
  // (each card is 320px; 8 cards = 2560px of content).
  await page.setViewportSize({ width: 800, height: 700 })

  const { token, auth } = await authenticate(request)
  const { projectId } = await setupProject(request, auth, 'pinned', 8)

  await loadAt(page, token, `/projects/${projectId}`)

  const backlogRow = page.locator('.kanban-column', { hasText: 'Backlog' })
  const header = backlogRow.locator('.kanban-column-header')
  await expect(header).toBeVisible({ timeout: 10_000 })

  // Wait until every card has finished its mount-grow animation; otherwise
  // the cards section briefly reports a sub-natural width and the row
  // can't yet scroll.
  await backlogRow
    .locator('.kanban-card')
    .first()
    .evaluate(async (el) => {
      const anims = el.getAnimations({ subtree: false })
      await Promise.all(anims.map((a) => a.finished.catch(() => undefined)))
    })

  // Capture the header's viewport-left before scrolling, then push the row
  // horizontally and confirm the label stayed pinned.
  const beforeBox = await header.boundingBox()
  expect(beforeBox, 'header bbox before scroll').toBeTruthy()

  const scrolled = await backlogRow.evaluate((el) => {
    el.scrollLeft = 400
    return el.scrollLeft
  })
  expect(scrolled, 'row actually scrolled horizontally').toBeGreaterThan(100)

  const afterBox = await header.boundingBox()
  expect(afterBox, 'header bbox after scroll').toBeTruthy()
  expect(
    Math.abs(afterBox!.x - beforeBox!.x),
    'sticky header x unchanged after row scroll',
  ).toBeLessThan(2)
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
