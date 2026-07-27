import { test, expect, type APIRequestContext, type Locator, type Page } from '@playwright/test'

/**
 * E2E for the usage trend chart's series encoding.
 *
 * The chart used to map series → line by hue alone, from the semantic UI
 * tokens (`--success`/`--danger` collapse under deuteranopia; `--accent-muted`
 * was 1.37:1 on `--surface`). It now uses a dedicated categorical ramp
 * (`--chart-1`..`--chart-6`, validated ≥ 3:1 on `--surface` in both themes)
 * *plus* a per-slot dash pattern that the legend swatch repeats — so a reader
 * who cannot separate the hues can still trace a line to its label.
 *
 * The trends endpoint is stubbed rather than seeded: a real mock:usage turn
 * produces one bucket per series, and a one-point series renders as a dot with
 * no line to carry a dash. Stubbing is also what makes six series (and the
 * seventh-series cap) reachable deterministically.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

/** Slot colours, in the fixed order `TrendsSection` hands them out. */
const CHART_TOKENS = [
  'var(--chart-1)',
  'var(--chart-2)',
  'var(--chart-3)',
  'var(--chart-4)',
  'var(--chart-5)',
  'var(--chart-6)',
]

/** The documented hex of each slot, per theme (`index.css`). */
const LIGHT_HEX = ['#006ebe', '#a43600', '#c03e86', '#967600', '#6731a8', '#00672c']
const DARK_HEX = ['#0071c3', '#c54300', '#db589e', '#a78400', '#a170eb', '#007f38']

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

/** Eight daily series (two more than the chart draws) of ten points each, so
 *  every drawn series is a polyline and the over-cap tail is exercised. */
function stubSeries() {
  const DAY = 86_400_000
  const base = Date.UTC(2026, 0, 5)
  return Array.from({ length: 8 }, (_, s) => ({
    metric: 'tokens',
    entity_id: `trend-series-${s + 1}`,
    points: Array.from({ length: 10 }, (_, i) => ({
      bucket_ts: base + i * DAY,
      // Descending totals so the widget's own ranking keeps slot order stable.
      tokens: 40_000 - s * 4_000 + i * 300 * (s + 1),
      est_cost: (40_000 - s * 4_000) / 100_000,
    })),
  }))
}

/** Read one attribute off every element a locator matches, in document order. */
const attrs = (loc: Locator, name: string): Promise<(string | null)[]> =>
  loc.evaluateAll<(string | null)[], string>(
    (els, n) => (els as Element[]).map((e) => e.getAttribute(n)),
    name,
  )

test('trend series are traceable without colour', async ({ request, page }) => {
  const token = await authenticate(request)
  await page.route('**/api/usage/trends**', (route) => route.fulfill({ json: stubSeries() }))

  await loadAt(page, token, '/usage')
  const chart = page.getByTestId('usage-trend-tokens-chart')
  await expect(chart).toBeVisible()

  // Only six lines are drawn: there is no seventh style, so the widget drops
  // the tail instead of reusing a hue.
  const lines = chart.locator('polyline')
  await expect(lines).toHaveCount(6)
  const legend = page.getByTestId('usage-trend-tokens-legend')
  await expect(legend.locator('.usage-legend-item')).toHaveCount(6)
  await expect(legend.locator('.usage-legend-more')).toHaveText('+2 more')

  // Channel 1 — hue, from the dedicated chart ramp and never a semantic token.
  const strokes = await attrs(lines, 'stroke')
  expect(strokes).toEqual(CHART_TOKENS)

  // Channel 2 — dash pattern, one per slot and all six distinct. (Slot 1 is
  // solid, i.e. no attribute at all, which is still a distinct value.)
  const dashes = await attrs(lines, 'stroke-dasharray')
  expect(new Set(dashes).size).toBe(6)

  // The legend swatch draws the series' own line, so the legend → chart
  // mapping survives with no colour vision at all.
  const swatchLines = legend.locator('.usage-legend-swatch line')
  await expect(swatchLines).toHaveCount(6)
  expect(await attrs(swatchLines, 'stroke')).toEqual(strokes)
  expect(await attrs(swatchLines, 'stroke-dasharray')).toEqual(dashes)

  // Every slot is named in the legend for assistive tech, not colour-coded only.
  const patterns = await attrs(legend.locator('.usage-legend-swatch'), 'data-pattern')
  expect(new Set(patterns).size).toBe(6)

  // The tokens resolve to the validated hexes in BOTH themes — a stray edit to
  // only one of the three theme blocks in index.css fails here.
  const resolve = () =>
    page.evaluate(() => {
      const s = getComputedStyle(document.documentElement)
      return [1, 2, 3, 4, 5, 6].map((i) => s.getPropertyValue(`--chart-${i}`).trim())
    })
  await page.evaluate(() => document.documentElement.setAttribute('data-theme', 'light'))
  expect(await resolve()).toEqual(LIGHT_HEX)
  await page.evaluate(() => document.documentElement.setAttribute('data-theme', 'dark'))
  expect(await resolve()).toEqual(DARK_HEX)
})
