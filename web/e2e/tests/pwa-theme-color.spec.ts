import { test, expect, type Page } from '@playwright/test'

// PWA status-bar color regression.
//
// On iPhone in standalone PWA mode the OS draws the status-bar area in
// whatever `<meta name="theme-color">` resolves to. We want it to match
// the nav rail's `--surface`, not the brand red — and to follow the
// user's explicit theme override from Settings when they have one.
//
// We can't observe the iOS status bar in headless Chromium, but we can
// assert the meta tags that drive it.

const SURFACE_LIGHT = '#ffffff'
const SURFACE_DARK = '#1a1d27'

async function themeColorMetas(page: Page) {
  return await page.evaluate(() =>
    Array.from(document.querySelectorAll('meta[name="theme-color"]')).map((m) => ({
      content: m.getAttribute('content'),
      media: m.getAttribute('media'),
    })),
  )
}

test.describe('PWA theme-color meta', () => {
  test('default page exposes light + dark media-conditioned tags matching --surface', async ({
    page,
  }) => {
    await page.goto('/')
    const metas = await themeColorMetas(page)
    // Look up by media query rather than index — the order is an
    // implementation detail.
    const light = metas.find((m) => m.media === '(prefers-color-scheme: light)')
    const dark = metas.find((m) => m.media === '(prefers-color-scheme: dark)')
    expect(light?.content?.toLowerCase()).toBe(SURFACE_LIGHT)
    expect(dark?.content?.toLowerCase()).toBe(SURFACE_DARK)
    // No stray un-mediated tag (e.g. leftover brand-red default).
    const unmediated = metas.filter((m) => !m.media)
    expect(unmediated).toHaveLength(0)
  })

  test('explicit dark theme override replaces metas with a single dark tag', async ({ page }) => {
    await page.addInitScript(() => {
      localStorage.setItem('peckboard_theme', 'dark')
    })
    await page.goto('/')
    // The App init effect runs after first paint; wait for it to swap
    // the metas before asserting.
    await expect
      .poll(async () => {
        const metas = await themeColorMetas(page)
        return metas.length === 1 && metas[0].media === null
          ? metas[0].content?.toLowerCase()
          : null
      })
      .toBe(SURFACE_DARK)
  })

  test('explicit light theme override replaces metas with a single light tag', async ({ page }) => {
    await page.addInitScript(() => {
      localStorage.setItem('peckboard_theme', 'light')
    })
    await page.goto('/')
    await expect
      .poll(async () => {
        const metas = await themeColorMetas(page)
        return metas.length === 1 && metas[0].media === null
          ? metas[0].content?.toLowerCase()
          : null
      })
      .toBe(SURFACE_LIGHT)
  })
})
