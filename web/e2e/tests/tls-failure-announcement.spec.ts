import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * A TLS startup failure is recorded as an announcement (fixed id
 * `tls-startup-failure`, kind `tls_error`) so the operator hears about it
 * at their next login instead of the fallback happening silently. This
 * test can't easily force the *real* startup TLS failure inside the
 * Playwright-managed server, so it exercises the same generic
 * announcements API and banner the backend uses, with the exact
 * title/message the TLS fallback path sends — verifying the contract
 * between `service::tls::announce_failure` and the frontend banner.
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

test('a TLS startup failure announcement renders as a dismissible banner at login', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)

  const createRes = await request.post('/api/announcements', {
    headers: auth,
    data: {
      kind: 'tls_error',
      title: 'HTTPS is disabled',
      message: 'Peckboard is serving plain HTTP only',
      detail: 'failed to read cert.pem: e2e test fixture',
    },
  })
  expect(createRes.ok(), `create announcement failed: ${await createRes.text()}`).toBeTruthy()

  await loadAt(page, token, '/')

  const banner = page.locator('.announcement-banner')
  await expect(banner).toBeVisible()
  await expect(banner).toContainText('HTTPS is disabled')
  await expect(banner).toContainText('Peckboard is serving plain HTTP only')

  await page.locator('.announcement-dismiss').click()
  await expect(banner).toHaveCount(0)

  // Dismissal deletes it server-side too, so it doesn't reappear next login.
  const listRes = await request.get('/api/announcements', { headers: auth })
  expect(listRes.ok()).toBeTruthy()
  const remaining = (await listRes.json()) as Array<{ title: string }>
  expect(remaining.some((a) => a.title === 'HTTPS is disabled')).toBe(false)
})
