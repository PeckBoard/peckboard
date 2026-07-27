import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * E2E for the usage chart's value readout.
 *
 * The trend chart used to render a shape with no recoverable numbers: no
 * hover, no keyboard traversal, only `yMin`/`yMax` labelled, no unit, and
 * `<text>` axis labels inside a `preserveAspectRatio="none"` viewBox (so the
 * glyphs stretched with the panel). The chart now keeps geometry in the SVG
 * and puts every glyph and marker in an HTML layer over it.
 *
 * The trends endpoint is stubbed rather than seeded: a real mock:usage turn
 * produces a single bucket per series, and one point cannot demonstrate a
 * readout MOVING under the arrow keys. Same approach as
 * usage-trend-series.spec.ts.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

/** Five daily points, each formatting to a distinct `fmtTokens` string. */
const TOKENS = [12_000, 24_000, 36_000, 48_000, 60_000]
const LABELS = ['12.0K', '24.0K', '36.0K', '48.0K', '60.0K']

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

/** Load a route in the browser already authenticated, by seeding the token
 *  the SPA reads from localStorage before any script runs. */
async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t as string)
  }, token)
  await page.goto(route)
}

/** One `overall` series of five daily points. Noon UTC keeps the local-time
 *  bucket label on the same calendar day in every plausible test TZ. */
function stubSeries() {
  const DAY = 86_400_000
  const base = Date.UTC(2026, 0, 5, 12)
  return [
    {
      metric: 'tokens',
      entity_id: 'overall',
      points: TOKENS.map((tokens, i) => ({
        bucket_ts: base + i * DAY,
        tokens,
        est_cost: tokens / 10_000,
      })),
    },
  ]
}

test('usage chart: hover and arrow keys read out per-point values with units', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await page.route('**/api/usage/trends**', (route) => route.fulfill({ json: stubSeries() }))

  await loadAt(page, token, '/usage')
  const chart = page.getByTestId('usage-trend-tokens-chart')
  await expect(chart).toBeVisible()

  // ── Axis text is HTML, never SVG <text>: nothing inside the non-uniformly
  //    scaled viewBox can be stretched into distorted glyphs. ──
  await expect(chart.locator('svg text')).toHaveCount(0)

  // ── All four gridlines are labelled, clear of the plot, in ascending
  //    order — not just the always-zero min and a max printed on the line. ──
  await expect(chart.locator('.usage-chart-tick')).toHaveText(['0', '20.0K', '40.0K', '60.0K'])

  // ── A unit caption per chart, so tokens and dollars can't be confused. ──
  await expect(page.getByTestId('usage-trend-tokens-chart-caption')).toHaveText('Tokens per day')
  await expect(page.getByTestId('usage-trend-cost-chart-caption')).toHaveText('USD per day')

  // ── Hover: the point nearest the pointer gets a readout with a formatted
  //    value and its unit. ──
  const plot = page.getByTestId('usage-trend-tokens-chart-plot')
  const box = await plot.boundingBox()
  expect(box, 'plot has a box').toBeTruthy()
  const midY = box!.height / 2

  const tooltip = page.getByTestId('usage-trend-tokens-chart-tooltip')
  await expect(tooltip).toHaveCount(0)

  await plot.hover({ position: { x: 3, y: midY } })
  await expect(tooltip).toBeVisible()
  await expect(tooltip).toContainText('Overall')
  await expect(tooltip).toContainText(LABELS[0])
  await expect(tooltip).toContainText('tokens')
  const firstDay = await tooltip.locator('.usage-chart-tooltip-x').innerText()

  // Hovering the far side lands on the last point, not the first.
  await plot.hover({ position: { x: box!.width - 3, y: midY } })
  await expect(tooltip).toContainText(LABELS[LABELS.length - 1])

  // ── Keyboard: the readout is reachable and traversable without a mouse. ──
  await plot.focus()
  await plot.press('Home')
  await expect(tooltip).toContainText(LABELS[0])
  await expect(tooltip.locator('.usage-chart-tooltip-x')).toHaveText(firstDay)

  for (let i = 1; i < LABELS.length; i++) {
    await plot.press('ArrowRight')
    await expect(tooltip).toContainText(LABELS[i])
  }
  // Past the last point the readout stays put instead of wrapping.
  await plot.press('ArrowRight')
  await expect(tooltip).toContainText(LABELS[LABELS.length - 1])

  await plot.press('ArrowLeft')
  await expect(tooltip).toContainText(LABELS[LABELS.length - 2])

  // The same values reach assistive tech through a live region, not only the
  // visual tooltip (which is aria-hidden).
  const live = chart.locator('.sr-only[role="status"]')
  await expect(live).toContainText(LABELS[LABELS.length - 2])
  await expect(live).toContainText('tokens')

  await plot.press('Escape')
  await expect(tooltip).toHaveCount(0)

  // ── Axis text must not distort with the panel: the same tick label has the
  //    same glyph size at 380px and at 1600px. An SVG <text> inside the
  //    preserveAspectRatio="none" viewBox would stretch with the width. ──
  const tick = chart.locator('.usage-chart-tick').last()
  const measure = async () => {
    const box = await tick.boundingBox()
    const fontSize = await tick.evaluate((el) => getComputedStyle(el).fontSize)
    return { height: Math.round(box!.height), fontSize }
  }
  await page.setViewportSize({ width: 380, height: 900 })
  await chart.scrollIntoViewIfNeeded()
  await expect(tick).toBeVisible()
  const narrow = await measure()
  await page.screenshot({ path: 'test-results/usage-chart-narrow.png' })

  await page.setViewportSize({ width: 1600, height: 900 })
  await chart.scrollIntoViewIfNeeded()
  await expect(tick).toBeVisible()
  const wide = await measure()
  await page.screenshot({ path: 'test-results/usage-chart-wide.png' })

  expect(narrow).toEqual(wide)
})
