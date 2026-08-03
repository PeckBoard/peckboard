import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * Overlapping `fetchUsage` batches must not commit out of order.
 *
 * Picking 30d and then 24h leaves two `Promise.all` batches in flight. Without
 * a request-id guard the earlier (slower) 30d batch lands last and overwrites
 * `dashboard` / `resolved` / `lastUpdated`, so the preset buttons say 24h while
 * the caption and every figure describe 30d — stamped "updated just now".
 *
 * Every usage request for a window wider than a week is held back here, which
 * forces exactly that interleaving: the 24h batch resolves first, the 30d batch
 * afterwards.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const DAY_MS = 86_400_000
const WEEK_MS = 7 * DAY_MS
/** Long enough that the 24h batch always commits first, short enough to keep
 *  the spec well inside the default timeout. */
/** `OPERATION_KINDS` in `store/usage.ts` — one `/api/usage/operations` request
 *  per kind, fired as the batch's last leg. */
const OPERATION_LEG_COUNT = 4
const SLOW_BATCH_MS = 2_500

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

/** `MM/DD`, matching `RangeBar`'s caption formatting. */
function mmdd(ms: number): string {
  const d = new Date(ms)
  return `${`${d.getMonth() + 1}`.padStart(2, '0')}/${`${d.getDate()}`.padStart(2, '0')}`
}

async function loadUsageAt24h(page: Page, token: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
    // Start on 24h so the first load is one of the fast (narrow-window)
    // requests — the delay below is reserved for the 30d batch under test.
    localStorage.setItem('peckboard_usage_range', JSON.stringify({ preset: '24h' }))
  }, token)
  await page.goto('/usage')
}

test('a slow 30d batch cannot overwrite the 24h window selected after it', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)

  // `fetchUsage` fans out the panel endpoints first and the four
  // `/api/usage/operations` legs only after those resolve, so the operations
  // count is what says the whole 30d batch has been released.
  let slowOperationLegs = 0
  await page.route(/\/api\/usage\//, async (route) => {
    const url = new URL(route.request().url())
    const from = Number(url.searchParams.get('from'))
    const to = Number(url.searchParams.get('to'))
    if (from && to && to - from > WEEK_MS) {
      await new Promise((resolve) => setTimeout(resolve, SLOW_BATCH_MS))
      if (url.pathname.endsWith('/api/usage/operations')) slowOperationLegs += 1
    }
    await route.continue()
  })

  await loadUsageAt24h(page, token)

  const caption = page.getByTestId('usage-range-caption')
  await expect(caption).toBeVisible({ timeout: 15_000 })
  await expect(page.getByTestId('usage-updated')).toBeVisible({ timeout: 15_000 })

  const now = Date.now()
  const day30 = mmdd(now - 30 * DAY_MS)
  const day24h = mmdd(now - DAY_MS)
  expect(day30, '30d and 24h windows start on different days').not.toBe(day24h)

  // Two range changes back to back: the 30d requests are still in flight when
  // the 24h ones are issued.
  await page.getByTestId('usage-range-30d').click()
  await page.getByTestId('usage-range-24h').click()

  // The 24h batch is fast, so it commits first.
  await expect(caption).toContainText(`Showing ${day24h} –`, { timeout: 15_000 })

  // Then let the held-back 30d batch land in full and assert it changed nothing.
  await expect
    .poll(() => slowOperationLegs, { timeout: 30_000 })
    .toBeGreaterThanOrEqual(OPERATION_LEG_COUNT)
  await page.waitForTimeout(2_000)
  await expect(caption).toContainText(`Showing ${day24h} –`)
  await expect(caption).not.toContainText(day30)
  await expect(caption).not.toContainText(day30)
  await expect(page.getByTestId('usage-range-24h')).toHaveAttribute('aria-pressed', 'true')
})
