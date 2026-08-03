import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * Live announcement broadcasts (ws.ts dispatches `peckboard:announcement`)
 * must reach every already-open client without a reload: a create shows
 * the banner live, and a dismissal on one client clears it on another.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type Auth = { token: string; auth: Record<string, string> }

async function authenticate(request: APIRequestContext): Promise<Auth> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

test('a live announcement create/dismiss reaches other open clients without reload', async ({
  browser,
  request,
}) => {
  const { token, auth } = await authenticate(request)

  const contextA = await browser.newContext()
  const contextB = await browser.newContext()
  const pageA = await contextA.newPage()
  const pageB = await contextB.newPage()

  await loadAt(pageA, token, '/')
  await loadAt(pageB, token, '/')

  const bannerA = pageA.locator('.announcement-banner')
  const bannerB = pageB.locator('.announcement-banner')
  await expect(bannerA).toHaveCount(0)
  await expect(bannerB).toHaveCount(0)

  const createRes = await request.post('/api/announcements', {
    headers: auth,
    data: {
      kind: 'info',
      title: 'Live announcement test',
      message: 'This should appear without a reload',
    },
  })
  expect(createRes.ok(), `create announcement failed: ${await createRes.text()}`).toBeTruthy()

  await expect(bannerA).toBeVisible()
  await expect(bannerA).toContainText('Live announcement test')
  await expect(bannerB).toBeVisible()
  await expect(bannerB).toContainText('Live announcement test')

  await pageA.locator('.announcement-dismiss').click()
  await expect(bannerA).toHaveCount(0)
  await expect(bannerB).toHaveCount(0)

  await contextA.close()
  await contextB.close()
})
