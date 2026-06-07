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
  const status = await request.get('/api/auth/status')
  const { has_users } = (await status.json()) as { has_users: boolean }
  const endpoint = has_users ? '/api/auth/login' : '/api/auth/register'
  const res = await request.post(endpoint, {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok()).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  cachedAuth = { token, auth: { Authorization: `Bearer ${token}` } }
  return cachedAuth
}

async function seedSession(request: APIRequestContext, auth: Record<string, string>) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-mob-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'mob', path: folderPath },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'mobile session', folder_id: folder.id },
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
})
