import { test, expect } from '@playwright/test'

// Appearance persistence regression.
//
// The theme and accent hue live in localStorage (`peckboard_theme`,
// `peckboard_hue`) and must be applied on page load by main.tsx's
// initAppearance() — before React renders. Previously the hue was only
// applied when the Settings page mounted, so a saved accent color
// silently reverted to the default until the user opened Settings.

test.describe('appearance persistence', () => {
  test('saved accent hue is applied on load without visiting Settings', async ({ page }) => {
    await page.addInitScript(() => {
      localStorage.setItem('peckboard_hue', '140')
    })
    await page.goto('/')
    const hue = await page.evaluate(() =>
      document.documentElement.style.getPropertyValue('--primary-hue'),
    )
    expect(hue).toBe('140')
  })

  test('saved dark theme sets data-theme on load', async ({ page }) => {
    await page.addInitScript(() => {
      localStorage.setItem('peckboard_theme', 'dark')
    })
    await page.goto('/')
    await expect
      .poll(() => page.evaluate(() => document.documentElement.getAttribute('data-theme')))
      .toBe('dark')
  })

  test('no saved values leaves defaults (no data-theme, default hue)', async ({ page }) => {
    await page.goto('/')
    const state = await page.evaluate(() => ({
      dataTheme: document.documentElement.getAttribute('data-theme'),
      inlineHue: document.documentElement.style.getPropertyValue('--primary-hue'),
      computedHue: getComputedStyle(document.documentElement)
        .getPropertyValue('--primary-hue')
        .trim(),
    }))
    expect(state.dataTheme).toBeNull()
    expect(state.computedHue).toBe('220')
  })

  test('invalid stored hue falls back to the default', async ({ page }) => {
    await page.addInitScript(() => {
      localStorage.setItem('peckboard_hue', '9999')
    })
    await page.goto('/')
    const hue = await page.evaluate(() =>
      document.documentElement.style.getPropertyValue('--primary-hue'),
    )
    expect(hue).toBe('220')
  })
})
