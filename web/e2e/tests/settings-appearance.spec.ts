import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * Settings → Appearance interactions, and the two-pane settings layout.
 *
 * The appearance page owns the per-browser presentation prefs: theme,
 * accent (preset swatches + free hue slider), font size, density and
 * motion. Each control must apply instantly (root attribute / inline
 * style / CSS var) AND persist through localStorage so initAppearance()
 * restores it on the next load. On desktop the section rail stays
 * visible, so switching sections needs no round-trip through a hub.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

async function openAppearance(page: Page, token: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/settings')
  await expect(page.getByTestId('settings-page')).toBeVisible({ timeout: 10_000 })
  await page.getByTestId('settings-nav-appearance').click()
}

test('accent preset swatch applies the hue and persists it', async ({ request, page }) => {
  const token = await authenticate(request)
  await openAppearance(page, token)

  const swatch = page.getByTestId('accent-swatch-145') // Parrot
  await expect(swatch).toBeVisible()
  await swatch.click()

  await expect(swatch).toHaveClass(/active/)
  const applied = await page.evaluate(() => ({
    hue: document.documentElement.style.getPropertyValue('--primary-hue'),
    stored: localStorage.getItem('peckboard_hue'),
  }))
  expect(applied.hue).toBe('145')
  expect(applied.stored).toBe('145')
})

test('font size, density and motion buttons apply and persist', async ({ request, page }) => {
  const token = await authenticate(request)
  await openAppearance(page, token)

  await page.getByTestId('font-size-large').click()
  await page.getByTestId('density-compact').click()
  await page.getByTestId('motion-reduced').click()

  const state = () =>
    page.evaluate(() => ({
      fontSize: document.documentElement.style.fontSize,
      density: document.documentElement.getAttribute('data-density'),
      motion: document.documentElement.getAttribute('data-motion'),
    }))
  expect(await state()).toEqual({ fontSize: '17px', density: 'compact', motion: 'reduce' })

  // Survives a reload without revisiting Settings.
  await page.goto('/')
  expect(await state()).toEqual({ fontSize: '17px', density: 'compact', motion: 'reduce' })

  // And each control resets cleanly.
  await page.goto('/settings')
  await page.getByTestId('settings-nav-appearance').click()
  await page.getByTestId('font-size-default').click()
  await page.getByTestId('density-comfortable').click()
  await page.getByTestId('motion-system').click()
  expect(await state()).toEqual({ fontSize: '', density: null, motion: null })
})

test('desktop rail switches sections directly and marks the open one', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await openAppearance(page, token)

  const appearanceNav = page.getByTestId('settings-nav-appearance')
  await expect(appearanceNav).toHaveAttribute('aria-current', 'true')
  await expect(page.getByTestId('accent-section')).toBeVisible()

  // No hub round-trip: the rail is still there, one click away.
  await page.getByTestId('settings-nav-chat').click()
  await expect(page.getByTestId('caveman-section')).toBeVisible()
  await expect(page.getByTestId('accent-section')).toHaveCount(0)
  await expect(page.getByTestId('settings-nav-chat')).toHaveAttribute('aria-current', 'true')
  await expect(appearanceNav).not.toHaveAttribute('aria-current', 'true')
})
