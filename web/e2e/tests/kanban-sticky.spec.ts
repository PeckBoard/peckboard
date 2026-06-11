import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Card row scrolling and section-locked sticky step headers inside the
 * kanban board.
 *
 * User-visible behaviours covered, one assertion each:
 *
 *   1. **Row scrolls, page doesn't** — on mobile, when a row has more
 *      cards than fit across the viewport, the cards stay on a single
 *      visual line and the row gets its own horizontal scrollbar. The
 *      page itself never scrolls horizontally.
 *   2. **Empty state** — a row with no cards shows a "No cards in …"
 *      placeholder so the row doesn't collapse to a thin strip.
 *   3. **Sticky header pin** — while scrolling vertically through a
 *      step's cards, that step's header pins flush under the toolbar.
 *      When the user scrolls past the step's bottom, the next step's
 *      header takes the slot; only one header is pinned at a time.
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
    data: {
      name: `wrap project ${suffix}`,
      folder_id: folder.id,
      worker_count: 0,
      workflow: 'task',
    },
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

test('cards form a single horizontal strip that scrolls inside the row, not the page', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  // Narrow viewport (below the 768px `md` breakpoint) so the board
  // renders the mobile horizontal-strip layout: cards stay on one line
  // and the row gets its own horizontal scroll. 8 cards × 280px =
  // 2240px of card content, far wider than the 700px viewport, so the
  // strip definitely needs to scroll.
  await page.setViewportSize({ width: 700, height: 700 })

  const { token, auth } = await authenticate(request)
  const { projectId } = await setupProject(request, auth, 'strip', 8)

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
  // scrollbar on the page itself.
  const hScroll = await page.evaluate(() => ({
    scrollWidth: document.documentElement.scrollWidth,
    clientWidth: document.documentElement.clientWidth,
  }))
  expect(hScroll.scrollWidth, 'document fits inside viewport width').toBeLessThanOrEqual(
    hScroll.clientWidth + 1,
  )

  // All cards sit on a single visual line: their tops align. The strip
  // itself absorbs the overflow via `overflow-x: auto`.
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
  expect(distinctRows, 'cards stay on a single horizontal line').toBe(1)

  // The strip itself owns the horizontal overflow — its scroll width
  // exceeds its client width because the cards don't fit.
  const strip = backlogRow.locator('.kanban-cards')
  const stripScroll = await strip.evaluate((el) => ({
    scrollWidth: el.scrollWidth,
    clientWidth: el.clientWidth,
  }))
  expect(stripScroll.scrollWidth, 'card strip scrolls horizontally inside the row').toBeGreaterThan(
    stripScroll.clientWidth,
  )
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

test('step header pins under the toolbar while scrolling its section, then yields to the next step', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  // Narrow + very short viewport so the board genuinely needs to
  // scroll vertically between adjacent step sections. Width is below
  // the `md` breakpoint (768px) so the board renders the mobile
  // horizontal-rows layout where steps stack vertically — that's the
  // layout the section-locked sticky header behaviour applies to.
  // The height drops to 280px because the new card design defaults to
  // collapsed (header-only) rows; the old 480px viewport left too
  // much room and the rows fit without scrolling.
  await page.setViewportSize({ width: 700, height: 280 })

  const { token, auth } = await authenticate(request)

  // Seed two rows directly so we don't have to drag-drop cards into
  // in_progress. The orchestrator is disabled (worker_count: 0).
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-stickyscroll-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-stickyscroll-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: `sticky scroll`,
      folder_id: folder.id,
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }
  // Twelve cards in backlog + twelve in in_progress → each row wraps to
  // enough lines that the board has a tall scroll range across the
  // step boundary.
  for (let i = 0; i < 12; i++) {
    const res = await request.post(`/api/projects/${project.id}/cards`, {
      headers: auth,
      data: { title: `Backlog ${i + 1}`, description: '', step: 'backlog', priority: 2 },
    })
    expect(res.ok(), `seed backlog ${i} failed: ${await res.text()}`).toBeTruthy()
  }
  for (let i = 0; i < 12; i++) {
    const res = await request.post(`/api/projects/${project.id}/cards`, {
      headers: auth,
      data: { title: `Progress ${i + 1}`, description: '', step: 'in_progress', priority: 2 },
    })
    expect(res.ok(), `seed progress ${i} failed: ${await res.text()}`).toBeTruthy()
  }

  await loadAt(page, token, `/projects/${project.id}`)

  // The step header pins flush beneath the project header — the board's
  // own `.kanban-board-header` is a non-scrolling sibling above the
  // `.kanban-board-scroll` scroll container (see kanban.css), so the
  // project header is the fixed reference the pinned column header
  // docks under.
  const toolbar = page.locator('.kanban-board-header')
  // Match rows by their header heading exactly so the locator can't
  // catch a priority-badge "Backlog" inside a card.
  const rowByLabel = (label: string) =>
    page.locator('.kanban-column').filter({
      has: page.locator('.kanban-column-header h3', { hasText: new RegExp(`^${label}$`) }),
    })
  const backlogRow = rowByLabel('Backlog')
  const inProgressRow = rowByLabel('In Progress')
  const backlogHeader = backlogRow.locator('.kanban-column-header')
  const inProgressHeader = inProgressRow.locator('.kanban-column-header')

  await expect(backlogRow.locator('.kanban-card-title', { hasText: /^Backlog 1$/ })).toBeVisible({
    timeout: 10_000,
  })
  await expect(
    inProgressRow.locator('.kanban-card-title', { hasText: /^Progress 1$/ }),
  ).toBeVisible()
  // Wait for the entry animations so geometry is stable.
  await backlogRow
    .locator('.kanban-card')
    .first()
    .evaluate(async (el) => {
      const anims = el.getAnimations({ subtree: false })
      await Promise.all(anims.map((a) => a.finished.catch(() => undefined)))
    })

  // Measure row positions inside the board's scroll container by
  // offsetTop, which is independent of current scrollTop.
  const geom = await page.evaluate(() => {
    const boardEl = document.querySelector('.kanban-board-scroll') as HTMLElement
    const rows = Array.from(boardEl.querySelectorAll('.kanban-column')) as HTMLElement[]
    const labelOf = (r: HTMLElement) =>
      r.querySelector('.kanban-column-header h3')?.textContent?.trim() ?? ''
    const findRow = (label: string) => rows.find((r) => labelOf(r) === label)!
    const backlog = findRow('Backlog')
    const inProgress = findRow('In Progress')
    return {
      maxScroll: boardEl.scrollHeight - boardEl.clientHeight,
      backlogTop: backlog.offsetTop,
      backlogBottom: backlog.offsetTop + backlog.offsetHeight,
      inProgressTop: inProgress.offsetTop,
      inProgressBottom: inProgress.offsetTop + inProgress.offsetHeight,
    }
  })
  expect(
    geom.maxScroll,
    'board has enough content to scroll across the backlog→in_progress boundary',
  ).toBeGreaterThan(geom.backlogBottom - geom.backlogTop)

  // 1) Mid-backlog: backlog header pinned, in-progress header below.
  const midBacklog = Math.min(
    geom.maxScroll,
    Math.round((geom.backlogTop + geom.backlogBottom) / 2),
  )
  await page.evaluate(async (y) => {
    const el = document.querySelector('.kanban-board-scroll') as HTMLElement
    el.scrollTo(0, y)
    await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)))
  }, midBacklog)
  const pinnedTop = await backlogHeader.evaluate((el) => el.getBoundingClientRect().top)
  const toolbarBottom = await toolbar.evaluate((el) => el.getBoundingClientRect().bottom)
  expect(
    Math.abs(pinnedTop - toolbarBottom),
    'backlog header pins flush under the toolbar while scrolling its section',
  ).toBeLessThan(3)
  const nextHeaderTop = await inProgressHeader.evaluate((el) => el.getBoundingClientRect().top)
  expect(nextHeaderTop, 'in-progress header not yet pinned').toBeGreaterThan(toolbarBottom + 10)

  // 2) Mid-in_progress: in-progress header pinned, backlog header gone.
  const midInProgress = Math.min(
    geom.maxScroll,
    Math.round((geom.inProgressTop + geom.inProgressBottom) / 2),
  )
  expect(
    midInProgress,
    'mid-in_progress scroll position must be past the backlog section',
  ).toBeGreaterThan(geom.backlogBottom - 1)
  await page.evaluate(async (y) => {
    const el = document.querySelector('.kanban-board-scroll') as HTMLElement
    el.scrollTo(0, y)
    await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)))
  }, midInProgress)
  const inPinnedTop = await inProgressHeader.evaluate((el) => el.getBoundingClientRect().top)
  const toolbarBottom2 = await toolbar.evaluate((el) => el.getBoundingClientRect().bottom)
  expect(
    Math.abs(inPinnedTop - toolbarBottom2),
    'in-progress header pins flush under the toolbar after passing backlog',
  ).toBeLessThan(3)
  const backlogTopAfter = await backlogHeader.evaluate((el) => el.getBoundingClientRect().top)
  expect(
    backlogTopAfter,
    'backlog header has scrolled off above the toolbar — only one header pinned',
  ).toBeLessThan(toolbarBottom2 - 1)
})
