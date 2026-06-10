import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * The kanban board's primary axis flips with viewport width:
 *
 *   - **Mobile (< 768px):** steps stack vertically as full-width rows;
 *     cards flow horizontally and wrap inside each row.
 *   - **Desktop (≥ 768px):** steps sit side by side as columns; cards
 *     stack top-to-bottom inside each column. (Covered by kanban-dnd
 *     and kanban-done-order at the default 1280×720 viewport.)
 *
 * This file pins the mobile orientation. A regression that breaks the
 * media-query flip would either ship a phone with desktop columns
 * crammed into 390px (unusable) or a desktop with mobile rows that
 * waste half the screen — both are user-visible and worth a test.
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
  suffix: string,
): Promise<{ projectId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-resp-${suffix}-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-resp-${suffix}-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: `responsive ${suffix}`,
      folder_id: folder.id,
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  // Park a card in backlog and one in in_progress so we can compare the
  // bounding boxes of two distinct step sections.
  for (const data of [
    { title: 'Backlog item', step: 'backlog' },
    { title: 'In-progress item', step: 'in_progress' },
  ]) {
    const res = await request.post(`/api/projects/${project.id}/cards`, {
      headers: auth,
      data: { ...data, description: '', priority: 2 },
    })
    expect(res.ok(), `seed ${data.title} failed: ${await res.text()}`).toBeTruthy()
  }
  return { projectId: project.id }
}

test('mobile viewport: kanban steps render as stacked rows (Backlog above In Progress)', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  // iPhone-class viewport: well below the 768px `md` breakpoint, so the
  // board must render the horizontal-rows layout.
  await page.setViewportSize({ width: 390, height: 844 })

  const { token, auth } = await authenticate(request)
  const { projectId } = await seedProject(request, auth, 'mobile-stack')

  await loadAt(page, token, `/projects/${projectId}`)

  const columnByLabel = (label: string) =>
    page.locator('.kanban-column').filter({
      has: page.locator('.kanban-column-header h3', { hasText: new RegExp(`^${label}$`) }),
    })
  const backlog = columnByLabel('Backlog')
  const inProgress = columnByLabel('In Progress')
  await expect(backlog.locator('.kanban-card-title', { hasText: 'Backlog item' })).toBeVisible({
    timeout: 10_000,
  })
  await expect(
    inProgress.locator('.kanban-card-title', { hasText: 'In-progress item' }),
  ).toBeVisible()

  const backlogBox = await backlog.boundingBox()
  const inProgressBox = await inProgress.boundingBox()
  expect(backlogBox && inProgressBox, 'both step sections measurable').toBeTruthy()

  // Stacked-rows assertion: In Progress sits below Backlog (not next to
  // it) and the two sections horizontally overlap (same column on the
  // page). If the desktop vertical-columns layout fired by mistake on a
  // 390px viewport, In Progress would be to the right of Backlog at the
  // same y, and the document would also overflow horizontally.
  expect(backlogBox!.y, 'Backlog above In Progress').toBeLessThan(inProgressBox!.y)
  expect(
    inProgressBox!.y,
    'In Progress starts at or below the bottom of Backlog (sections do not overlap vertically)',
  ).toBeGreaterThanOrEqual(backlogBox!.y + backlogBox!.height - 1)
  const backlogXRange = [backlogBox!.x, backlogBox!.x + backlogBox!.width]
  const inProgXRange = [inProgressBox!.x, inProgressBox!.x + inProgressBox!.width]
  expect(
    Math.min(backlogXRange[1], inProgXRange[1]) - Math.max(backlogXRange[0], inProgXRange[0]),
    'sections share the page x-axis (stacked rows, not side-by-side columns)',
  ).toBeGreaterThan(0)

  // The page must not overflow horizontally — a mobile-rows layout never
  // forces the document wider than the viewport.
  const docWidths = await page.evaluate(() => ({
    scrollWidth: document.documentElement.scrollWidth,
    clientWidth: document.documentElement.clientWidth,
  }))
  expect(docWidths.scrollWidth, 'document fits inside mobile viewport width').toBeLessThanOrEqual(
    docWidths.clientWidth + 1,
  )
})
