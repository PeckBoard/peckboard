import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Document Review on a phone: the bottom action bar instead of the rail,
 * the panels as sheets, and a long press instead of a click to annotate.
 *
 * Same deterministic engine as the desktop spec — `mock:doc-review` calls
 * the real MCP review tools — and the same rule: every review that runs a
 * pass is armed with the mock model first (`armReviewer`), because an
 * unpinned review session resolves to auto → a real Claude model.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const DOC = [
  '# Onboarding', // 1
  '', // 2
  'The team ships on Fridays.', // 3
  '', // 4
  '## On-call', // 5
  '', // 6
  'On-call rotates weekly.', // 7
  '',
].join('\n')

type WsEvent = { kind: string }

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function seedFolder(
  request: APIRequestContext,
  auth: Record<string, string>,
  suffix: string,
): Promise<{ id: string; dir: string }> {
  const dir = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-mreview-${suffix}-`))
  mkdirSync(path.join(dir, 'docs'), { recursive: true })
  writeFileSync(path.join(dir, 'docs', 'onboarding.md'), DOC)
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-mreview-${suffix}-${Date.now()}`, path: dir },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return { id: ((await res.json()) as { id: string }).id, dir }
}

async function createReview(
  request: APIRequestContext,
  auth: Record<string, string>,
  folderId: string,
  title: string,
): Promise<string> {
  const res = await request.post('/api/doc-reviews', {
    headers: auth,
    data: { source_kind: 'file', source_ref: `${folderId}:docs/onboarding.md`, title },
  })
  expect(res.ok(), `create review failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { review: { id: string } }).review.id
}

/** Create the review session pinned to the mock reviewer, and wait for that
 *  warm-up turn to finish. */
async function armReviewer(
  request: APIRequestContext,
  auth: Record<string, string>,
  reviewId: string,
): Promise<string> {
  const res = await request.post(`/api/doc-reviews/${reviewId}/pass`, {
    headers: auth,
    data: {
      message: '[mock:chat] warming up the reviewer',
      include_annotations: false,
      model: 'mock:doc-review',
    },
  })
  expect(res.ok(), `arm pass failed: ${await res.text()}`).toBeTruthy()
  const { session_id: sessionId } = (await res.json()) as { session_id: string }
  const session = await request.get(`/api/sessions/${sessionId}`, { headers: auth })
  expect((await session.json()).model, 'the review session must run on the mock provider').toBe(
    'mock:doc-review',
  )
  await expect
    .poll(
      async () => {
        const events = await request.get(`/api/sessions/${sessionId}/events?limit=1000`, {
          headers: auth,
        })
        if (!events.ok()) return 0
        return ((await events.json()) as WsEvent[]).filter((e) => e.kind === 'agent-end').length
      },
      { timeout: 20_000, message: 'the warm-up turn never finished' },
    )
    .toBeGreaterThan(0)
  return sessionId
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto(route)
}

/**
 * Hold a block the way a thumb does. DocPane opens the annotation sheet on
 * a 450ms hold (a tap is left free for scrolling and text selection), so the
 * press is a real `touchstart` and the release only happens once the sheet
 * is up — no sleeps, the popover assertion is the wait.
 */
async function pressBlock(page: Page, line: number) {
  const selector = `[data-testid="review-block"][data-line-start="${line}"]`
  await expect(page.locator(selector).first()).toBeVisible()
  await page.evaluate((sel) => {
    const el = document.querySelector(sel) as HTMLElement | null
    if (!el) throw new Error(`no block for ${sel}`)
    const rect = el.getBoundingClientRect()
    const touch = new Touch({
      identifier: 1,
      target: el,
      clientX: rect.left + Math.min(20, rect.width / 2),
      clientY: rect.top + Math.min(10, rect.height / 2),
    })
    el.dispatchEvent(
      new TouchEvent('touchstart', {
        bubbles: true,
        cancelable: true,
        touches: [touch],
        targetTouches: [touch],
        changedTouches: [touch],
      }),
    )
  }, selector)
  await expect(page.getByTestId('review-popover')).toBeVisible({ timeout: 10_000 })
  await page.evaluate((sel) => {
    const el = document.querySelector(sel) as HTMLElement | null
    if (!el) return
    const rect = el.getBoundingClientRect()
    const touch = new Touch({
      identifier: 1,
      target: el,
      clientX: rect.left + Math.min(20, rect.width / 2),
      clientY: rect.top + Math.min(10, rect.height / 2),
    })
    el.dispatchEvent(
      new TouchEvent('touchend', {
        bubbles: true,
        cancelable: true,
        touches: [],
        targetTouches: [],
        changedTouches: [touch],
      }),
    )
  }, selector)
}

/** Leave no reviews behind — an open review owns a tab chip, and the
 *  tab-strip specs later in the run count the chips in the strip. */
async function cleanupReviews(request: APIRequestContext, auth: Record<string, string>) {
  const res = await request.get('/api/doc-reviews', { headers: auth })
  if (!res.ok()) return
  for (const r of ((await res.json()) as { reviews: Array<{ id: string }> }).reviews) {
    await request.delete(`/api/doc-reviews/${r.id}`, { headers: auth })
  }
}

test.describe('document review — mobile', () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true })

  test.afterEach(async ({ request }) => {
    const { auth } = await authenticate(request)
    await cleanupReviews(request, auth)
  })

  test('the wizard works at 390px and the review screen swaps the rail for a bottom bar', async ({
    request,
    page,
  }) => {
    const { token, auth } = await authenticate(request)
    const folder = await seedFolder(request, auth, 'wizard')

    await loadAt(page, token, '/review')
    await expect(page.getByTestId('review-list')).toBeVisible({ timeout: 15_000 })
    await page.getByTestId('review-new').click()
    await expect(page.getByTestId('review-wizard')).toBeVisible()
    await page.getByTestId('review-wizard-kind-file').click()
    await page.getByTestId('review-wizard-next').click()
    // Pick THIS test's folder explicitly: the select defaults to whichever
    // folder is first in the workspace, and a full suite run has dozens.
    await page.getByTestId('review-wizard-folder').selectOption(folder.id)
    await page.getByTestId('review-wizard-file').click()
    await page.getByRole('option', { name: /docs\/onboarding\.md/ }).click()
    await page.getByTestId('review-wizard-title').fill('Phone review')
    const create = page.getByTestId('review-wizard-create')
    // The modal has to fit: the create button must be reachable, not clipped
    // off the bottom of a 844px-tall viewport.
    await expect(create).toBeInViewport()
    await create.click()

    await expect(page.getByTestId('review-view')).toBeVisible({ timeout: 15_000 })
    await expect(page.getByTestId('review-mobile-bar')).toBeVisible()
    await expect(page.getByTestId('review-mobile-bar')).toBeInViewport()
    // The desktop rail is gone below the breakpoint — one surface, not two.
    await expect(page.locator('.review-rail')).toHaveCount(0)
    await expect(page.getByTestId('review-doc')).toContainText('On-call rotates weekly.')
  })

  test('the panel sheets open from the bottom bar and close on Escape and on the backdrop', async ({
    request,
    page,
  }) => {
    const { token, auth } = await authenticate(request)
    const folder = await seedFolder(request, auth, 'sheets')
    const reviewId = await createReview(request, auth, folder.id, 'Sheet review')
    await armReviewer(request, auth, reviewId)

    await loadAt(page, token, `/review/${reviewId}`)
    await expect(page.getByTestId('review-mobile-bar')).toBeVisible({ timeout: 15_000 })

    // Annotations sheet → Escape.
    await page.getByTestId('review-tab-annotations').click()
    await expect(page.getByTestId('review-sheet-annotations')).toBeVisible()
    await expect(page.getByTestId('review-annotation-rail')).toBeVisible()
    await page.keyboard.press('Escape')
    await expect(page.getByTestId('review-sheet-annotations')).toHaveCount(0)

    // History sheet → backdrop tap.
    await page.getByTestId('review-tab-history').click()
    await expect(page.getByTestId('review-sheet-history')).toBeVisible()
    await expect(page.getByTestId('review-history')).toBeVisible()
    await page.locator('.review-sheet__backdrop').click({ position: { x: 5, y: 5 } })
    await expect(page.getByTestId('review-sheet-history')).toHaveCount(0)

    // Chat sheet: the composer stays on screen once focused — the sheet is
    // sized off --app-height, so the keyboard can't bury it.
    await page.getByTestId('review-tab-chat').click()
    await expect(page.getByTestId('review-sheet-chat')).toBeVisible()
    const input = page.getByTestId('review-chat-input')
    await input.click()
    await expect(input).toBeFocused()
    await expect(input).toBeInViewport()
    await expect(page.getByTestId('review-chat-send')).toBeInViewport()
    await page.getByTestId('review-sheet-close').click()
    await expect(page.getByTestId('review-sheet-chat')).toHaveCount(0)
  })

  test('a long press annotates, the bottom bar runs the pass, and a question is answerable', async ({
    request,
    page,
  }) => {
    const { token, auth } = await authenticate(request)
    const folder = await seedFolder(request, auth, 'pass')
    const reviewId = await createReview(request, auth, folder.id, 'Touch review')
    await armReviewer(request, auth, reviewId)

    await loadAt(page, token, `/review/${reviewId}`)
    await expect(page.getByTestId('review-mobile-bar')).toBeVisible({ timeout: 15_000 })

    // Hold the passage → the annotation sheet with the six verbs.
    await pressBlock(page, 3)
    await page.getByTestId('review-popover-comment').click()
    await page.getByTestId('review-annotation-editor').fill('we ship on Tuesdays')
    await page.getByTestId('review-annotation-submit').click()
    await expect(page.getByTestId('review-popover')).toHaveCount(0)

    // The queued count shows on the bar, and Run pass lives there too.
    const runPass = page.getByTestId('review-run-pass')
    await expect(runPass).toBeEnabled()
    await runPass.click()
    await expect(page.getByTestId('review-version')).toHaveText('v2', { timeout: 25_000 })
    await expect(page.getByTestId('review-doc')).toContainText('Mock reviewer pass 2.')

    // A second pass, this one asking: the card has to be usable on a phone
    // with the document still behind it.
    await pressBlock(page, 7)
    await page.getByTestId('review-popover-wrong').click()
    await page.getByTestId('review-annotation-editor').fill('[mock:ask] weekly or daily?')
    await page.getByTestId('review-annotation-submit').click()
    await expect(page.getByTestId('review-popover')).toHaveCount(0)
    await page.getByTestId('review-run-pass').click()

    const card = page.getByTestId('review-question-card')
    await expect(card).toBeVisible({ timeout: 25_000 })
    await expect(card).toBeInViewport()
    await expect(page.getByTestId('review-doc')).toBeVisible()
    await expect(page.getByTestId('review-status')).toHaveAttribute('data-status', 'needs_input')
    await card.getByRole('radio', { name: 'Rewrite it' }).click()
    await page.getByTestId('review-question-submit').click()
    await expect(page.getByTestId('review-version')).toHaveText('v3', { timeout: 25_000 })
    await expect(page.getByTestId('review-question-card')).toHaveCount(0)
  })
})
