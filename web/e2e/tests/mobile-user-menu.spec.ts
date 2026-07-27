import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * Mobile user-menu (avatar dropdown) regression test.
 *
 * Pins the bug where tapping the avatar in the top rail on a phone
 * viewport did not show the menu. The dropdown must render, be fully
 * inside the viewport, and its items must be tappable.
 */

const E2E_USER = process.env.PECKBOARD_E2E_USER ?? 'e2e-user'
const E2E_PASS = process.env.PECKBOARD_E2E_PASS ?? 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok()).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })
}

// Same mobile emulation as mobile-layout.spec.ts: phone viewport +
// mobile UA + touch so media queries and touch handlers both fire.
test.use({
  viewport: { width: 390, height: 844 },
  userAgent:
    'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1',
  isMobile: true,
  hasTouch: true,
})

test.describe('mobile user menu', () => {
  test('tapping the avatar opens the dropdown inside the viewport', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const token = await authenticate(request)
    await loadAt(page, token, '/')

    const avatar = page.getByRole('button', { name: 'User menu' })
    await expect(avatar).toBeVisible()
    await avatar.tap()

    const dropdown = page.locator('.user-menu-dropdown')
    await expect(dropdown).toBeVisible()
    await expect(dropdown.getByRole('menuitem', { name: 'Sign out' })).toBeVisible()

    // The dropdown must be portaled to <body>: WebKit clips fixed
    // descendants of the rail's mobile overflow-x scroller (WebKit bug
    // 160953), which made the menu invisible on iPhone. Rendering at
    // body level is the fix — pin it so a refactor can't regress it.
    const portaled = await dropdown.evaluate((el) => el.parentElement === document.body)
    expect(portaled, 'dropdown is a direct child of <body>').toBe(true)

    // Fully inside the viewport — a dropdown parked off-screen renders
    // "visible" to Playwright but is unreachable on a real phone.
    const box = await dropdown.boundingBox()
    expect(box).toBeTruthy()
    const vp = page.viewportSize()!
    expect(box!.x).toBeGreaterThanOrEqual(0)
    expect(box!.y).toBeGreaterThanOrEqual(0)
    expect(box!.x + box!.width).toBeLessThanOrEqual(vp.width)
    expect(box!.y + box!.height).toBeLessThanOrEqual(vp.height)

    // And the top item actually receives the tap (nothing overlays it).
    await dropdown.getByRole('menuitem', { name: 'Settings' }).tap()
    await expect(dropdown).toBeHidden()
  })
})
