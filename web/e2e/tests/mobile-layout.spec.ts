import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Mobile viewport regression tests.
 *
 * Pins two bugs that broke the phone experience after the tab-strip
 * refactor: the page itself was scrolling instead of the inner panes,
 * and pinch-zoom could resize the UI.
 *
 * We don't need a separate "iPhone vs Android" matrix here — these
 * assertions are about CSS overflow + the viewport meta tag, both of
 * which behave the same in any modern mobile webview.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

let cachedAuth: { token: string; auth: Record<string, string> } | null = null

async function authenticate(request: APIRequestContext) {
  if (cachedAuth) return cachedAuth
  // The server auto-bootstraps the admin from PECKBOARD_BOOTSTRAP_*
  // env vars at first start (see playwright.config.ts); we just log in.
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok()).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  cachedAuth = { token, auth: { Authorization: `Bearer ${token}` } }
  return cachedAuth
}

async function seedSession(
  request: APIRequestContext,
  auth: Record<string, string>,
  opts: { model?: string } = {},
) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-mob-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'mob', path: folderPath },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'mobile session', folder_id: folder.id, model: opts.model },
  })
  expect(sessionRes.ok()).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  return session.id
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })
}

// Force a phone-sized viewport with mobile UA so CSS media queries
// fire correctly. We stay on Chromium (the only browser the e2e
// suite installs) instead of WebKit because the layout assertions
// here are about HTML/CSS, not WebKit-specific behaviour.
test.use({
  viewport: { width: 390, height: 844 },
  userAgent:
    'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1',
  isMobile: true,
  hasTouch: true,
})

test.describe('mobile layout', () => {
  test('viewport meta disables user-scaling', async ({ page, baseURL }) => {
    expect(baseURL).toBeTruthy()
    await page.goto('/')
    const content = await page.locator('meta[name="viewport"]').getAttribute('content')
    expect(content, 'viewport meta tag present').toBeTruthy()
    expect(content).toMatch(/user-scalable\s*=\s*no/)
    expect(content).toMatch(/maximum-scale\s*=\s*1/)
  })

  test('page does not horizontally overflow on a session view', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    const sessionId = await seedSession(request, auth)

    await loadAt(page, token, `/sessions/${sessionId}`)

    // Body / root must not be wider than the viewport. If a child
    // element forced the layout wider (e.g. tab strip without
    // max-width, long unwrapped text) this fails.
    const widths = await page.evaluate(() => ({
      docScroll: document.documentElement.scrollWidth,
      docClient: document.documentElement.clientWidth,
      bodyScroll: document.body.scrollWidth,
      bodyClient: document.body.clientWidth,
      innerWidth: window.innerWidth,
    }))
    expect(widths.docScroll).toBeLessThanOrEqual(widths.innerWidth)
    expect(widths.bodyScroll).toBeLessThanOrEqual(widths.innerWidth)
  })

  test('body and #root do not vertically scroll (panes handle it)', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    const sessionId = await seedSession(request, auth)

    await loadAt(page, token, `/sessions/${sessionId}`)

    const overflow = await page.evaluate(() => ({
      htmlOverflow: getComputedStyle(document.documentElement).overflow,
      bodyOverflow: getComputedStyle(document.body).overflow,
      rootOverflow: (() => {
        const r = document.getElementById('root')
        return r ? getComputedStyle(r).overflow : null
      })(),
    }))
    expect(overflow.htmlOverflow).toContain('hidden')
    expect(overflow.bodyOverflow).toContain('hidden')
    expect(overflow.rootOverflow).toContain('hidden')
  })

  test('top nav is left-aligned (not centered) on phone widths', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    const sessionId = await seedSession(request, auth)

    await loadAt(page, token, `/sessions/${sessionId}`)

    // The brand "P" tile should hug the left edge of the rail. Allow
    // for the rail's own 8px horizontal padding (defined in App.css).
    const railBox = await page.locator('.rail').boundingBox()
    const brandBox = await page.locator('.rail-brand').boundingBox()
    expect(railBox).not.toBeNull()
    expect(brandBox).not.toBeNull()
    if (!railBox || !brandBox) return
    const leftGap = brandBox.x - railBox.x
    expect(leftGap, `brand should sit near the left edge, was ${leftGap}px in`).toBeLessThan(24)
  })

  test('focusing an input pins window scroll to 0 (keyboard does not shove the page down)', async ({
    request,
    page,
    baseURL,
  }) => {
    // Regression test: when the soft keyboard opened on iOS Safari, the
    // window ended up at scrollY > 0 with the top toolbar shoved off
    // screen. We pin scroll back to 0 whenever an editable element is
    // focused. Playwright can't open a real soft keyboard, so we
    // simulate the symptom — programmatic `window.scrollTo` while an
    // input has focus — and assert the handler pins it back.
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    const sessionId = await seedSession(request, auth)

    await loadAt(page, token, `/sessions/${sessionId}`)
    const input = page.locator('.input-textarea')
    await expect(input).toBeVisible({ timeout: 5_000 })
    await input.click()
    // Confirm focus actually landed — `click` is what reliably moves
    // `document.activeElement` to the textarea under mobile emulation;
    // `focus()` alone misses it.
    await expect
      .poll(async () => page.evaluate(() => document.activeElement?.tagName), {
        timeout: 2_000,
      })
      .toBe('TEXTAREA')

    // Provoke a window scroll like the iOS keyboard would. Without the
    // pin, the page stays scrolled; with it, our `pinScrollIfFocused`
    // listener resets to (0,0) on the next tick.
    await page.evaluate(() => {
      // Force the page to be scrollable for the duration of the test so
      // `window.scrollTo` actually changes scrollY (our CSS pins
      // `overflow: hidden` on html, which would otherwise no-op the
      // scroll and make the regression undetectable here).
      document.documentElement.style.setProperty('overflow', 'auto', 'important')
      document.body.style.setProperty('overflow', 'auto', 'important')
      document.body.style.minHeight = '4000px'
      window.scrollTo(0, 500)
    })

    // Give the scroll listener a tick to fire.
    await page.waitForFunction(() => window.scrollY === 0, undefined, { timeout: 2_000 })
    expect(await page.evaluate(() => window.scrollY)).toBe(0)
  })

  test('tapping Send does not blur the textarea (keyboard stays open, single-tap works)', async ({
    request,
    page,
    baseURL,
  }) => {
    // Regression test: on mobile, tapping Send used to blur the textarea,
    // which closed the soft keyboard and shifted the input bar down before
    // the click landed — so the first tap was wasted and users had to tap
    // a second time. The fix is `preventDefault` on the button's
    // pointerdown so focus stays on the textarea through the tap. We can't
    // open a real soft keyboard in Playwright, but the load-bearing
    // property — textarea retains focus through the Send tap — is
    // directly observable here.
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    const sessionId = await seedSession(request, auth, { model: 'mock:happy-path' })

    await loadAt(page, token, `/sessions/${sessionId}`)
    const input = page.locator('.input-textarea')
    const send = page.locator('.send-btn')
    await expect(input).toBeVisible({ timeout: 5_000 })

    await input.tap()
    await input.fill('hello')
    // Confirm focus + that Send is now enabled before we tap it.
    await expect
      .poll(async () => page.evaluate(() => document.activeElement?.tagName), { timeout: 2_000 })
      .toBe('TEXTAREA')
    await expect(send).toBeEnabled()

    await send.tap()

    // The key assertion: the textarea must still be the active element
    // immediately after the tap. If pointerdown had been allowed to shift
    // focus to the button, this would be 'BUTTON' (briefly) or 'BODY'
    // (once the button disables itself in handleSend) — both of which
    // would have closed the soft keyboard on a real device.
    const active = await page.evaluate(() => document.activeElement?.tagName)
    expect(active).toBe('TEXTAREA')

    // And the send actually fired: the textarea clears.
    await expect(input).toHaveValue('', { timeout: 5_000 })
  })
})
