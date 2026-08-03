import { test, expect } from '@playwright/test'

// Appearance persistence regression.
//
// Theme, accent hue, font size, density and motion live in localStorage
// (`peckboard_theme`, `peckboard_hue`, `peckboard_font_size`,
// `peckboard_density`, `peckboard_motion`) and must be applied on page
// load by main.tsx's initAppearance() — before React renders. Previously
// the hue was only applied when the Settings page mounted, so a saved
// accent color silently reverted to the default until the user opened
// Settings.

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

  test('saved font size scales the root on load', async ({ page }) => {
    await page.addInitScript(() => {
      localStorage.setItem('peckboard_font_size', 'large')
    })
    await page.goto('/')
    const size = await page.evaluate(() => document.documentElement.style.fontSize)
    expect(size).toBe('17px')
  })

  test('default font size leaves the root alone', async ({ page }) => {
    await page.goto('/')
    const size = await page.evaluate(() => document.documentElement.style.fontSize)
    expect(size).toBe('')
  })

  test('saved compact density sets data-density on load', async ({ page }) => {
    await page.addInitScript(() => {
      localStorage.setItem('peckboard_density', 'compact')
    })
    await page.goto('/')
    await expect
      .poll(() => page.evaluate(() => document.documentElement.getAttribute('data-density')))
      .toBe('compact')
  })

  test('saved reduced motion sets data-motion on load', async ({ page }) => {
    await page.addInitScript(() => {
      localStorage.setItem('peckboard_motion', 'reduced')
    })
    await page.goto('/')
    await expect
      .poll(() => page.evaluate(() => document.documentElement.getAttribute('data-motion')))
      .toBe('reduce')
  })

  test('system motion follows the OS preference', async ({ page }) => {
    await page.emulateMedia({ reducedMotion: 'reduce' })
    await page.goto('/')
    await expect
      .poll(() => page.evaluate(() => document.documentElement.getAttribute('data-motion')))
      .toBe('reduce')
    // Preference lifts → attribute comes off (the matchMedia listener).
    await page.emulateMedia({ reducedMotion: 'no-preference' })
    await expect
      .poll(() => page.evaluate(() => document.documentElement.getAttribute('data-motion')))
      .toBeNull()
  })
})
