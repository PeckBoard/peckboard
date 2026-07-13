/**
 * e2e tests for the Backup section in Settings → Server.
 *
 * The bootstrapped user is an admin, so the backup section must be visible.
 * The download endpoint is intercepted so no real archive is served.
 */

import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

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

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

test('Settings → Server shows Backup section for admin user', async ({ request, page }) => {
  const token = await authenticate(request)
  await loadAt(page, token, '/settings')

  const settingsPage = page.getByTestId('settings-page')
  await expect(settingsPage).toBeVisible({ timeout: 10_000 })

  await settingsPage.getByTestId('settings-nav-server').click()

  const backupSection = settingsPage.getByTestId('backup-section')
  await expect(backupSection).toBeVisible()
  await expect(backupSection).toContainText('Backup')

  const downloadBtn = settingsPage.getByTestId('backup-download-btn')
  await expect(downloadBtn).toBeVisible()
})

test('Download backup button triggers GET /api/admin/backup', async ({ request, page }) => {
  const token = await authenticate(request)

  // Intercept the backup endpoint before the page loads so the handler is
  // in place when the button is clicked.
  let backupRequested = false
  await page.route('**/api/admin/backup', async (route) => {
    if (route.request().method() === 'GET' && !route.request().url().includes('/status')) {
      backupRequested = true
      await route.fulfill({
        status: 200,
        headers: {
          'Content-Type': 'application/gzip',
          'Content-Disposition': 'attachment; filename="peckboard-backup-test.tar.gz"',
        },
        body: Buffer.from([0x1f, 0x8b, 0x08, 0x00]), // minimal gzip-ish bytes
      })
    } else {
      await route.continue()
    }
  })

  await loadAt(page, token, '/settings')

  const settingsPage = page.getByTestId('settings-page')
  await expect(settingsPage).toBeVisible({ timeout: 10_000 })

  await settingsPage.getByTestId('settings-nav-server').click()
  await expect(settingsPage.getByTestId('backup-section')).toBeVisible()

  // Click the download button and wait for the intercepted request
  const requestPromise = page.waitForRequest(
    (req) => req.url().includes('/api/admin/backup') && !req.url().includes('/status'),
  )
  await settingsPage.getByTestId('backup-download-btn').click()
  await requestPromise

  expect(backupRequested).toBe(true)
})
