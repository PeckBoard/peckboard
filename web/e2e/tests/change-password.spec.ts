import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * E2E for password changes.
 *
 * Two distinct user-visible flows, each covered once:
 *   1. Self-service: signed-in user clicks the avatar in the rail, picks
 *      "Change password", confirms current + new + confirm, and stays
 *      signed in with a fresh token. New password works on a fresh login
 *      and the old one stops working.
 *   2. Admin reset: admin opens the user-management page, clicks
 *      "Reset password" on a row, sets a new one, and the target user
 *      can log in with it (without the admin knowing the old one).
 *
 * Both tests create throwaway users via the admin API so neither one
 * disturbs the bootstrap-admin credentials other specs rely on.
 */

const ADMIN_USER = 'e2e-user'
const ADMIN_PASS = 'e2e-password-1234'

let cachedAdminAuth: { token: string; auth: Record<string, string> } | null = null

/** Authenticate as the bootstrap admin once per spec file. The per-IP
 *  login limiter sees the whole suite as one client, so re-logging in
 *  for every test would chew through the budget. */
async function authenticateAdmin(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  if (cachedAdminAuth) return cachedAdminAuth
  const res = await request.post('/api/auth/login', {
    data: { username: ADMIN_USER, password: ADMIN_PASS },
  })
  expect(res.ok(), `admin login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  cachedAdminAuth = { token, auth: { Authorization: `Bearer ${token}` } }
  return cachedAdminAuth
}

/** Mint a throwaway user via the admin API and return its credentials. */
async function createThrowawayUser(
  request: APIRequestContext,
  adminAuth: Record<string, string>,
  suffix: string,
): Promise<{ id: string; username: string; password: string }> {
  // Timestamp + suffix → unique across runs even if the server's data
  // dir is reused (playwright.config.ts only wipes it per CI run).
  const username = `pw-test-${suffix}-${Date.now()}`
  const password = 'orig-password-1234'
  const res = await request.post('/api/users', {
    headers: adminAuth,
    data: { username, password, role: 'user' },
  })
  expect(res.ok(), `create user failed: ${await res.text()}`).toBeTruthy()
  const user = (await res.json()) as { id: string }
  return { id: user.id, username, password }
}

/** Log in as a user via the API and return their bearer token. */
async function loginAs(
  request: APIRequestContext,
  username: string,
  password: string,
): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username, password },
  })
  expect(res.ok(), `login as ${username} failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

/** Plant a token in localStorage and load the SPA at the given route. */
async function loadAs(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
  // The rail (with the avatar) is always rendered once auth resolves.
  await expect(page.locator('.rail-avatar')).toBeVisible({ timeout: 10_000 })
}

test.describe('change password', () => {
  test('a signed-in user can change their own password from the avatar dropdown', async ({
    request,
    page,
  }) => {
    const { auth: adminAuth } = await authenticateAdmin(request)
    const u = await createThrowawayUser(request, adminAuth, 'self')
    const newPassword = 'fresh-password-9876'

    const token = await loginAs(request, u.username, u.password)
    await loadAs(page, token, '/')

    // Open the avatar dropdown and pick "Change password".
    await page.locator('.rail-avatar').click()
    const menu = page.locator('.user-menu-dropdown')
    await expect(menu).toBeVisible()
    await menu.getByRole('menuitem', { name: 'Change password' }).click()

    // Fill out the modal.
    const modal = page.locator('.modal')
    await expect(modal).toBeVisible()
    await modal.locator('#cp-current').fill(u.password)
    await modal.locator('#cp-new').fill(newPassword)
    await modal.locator('#cp-confirm').fill(newPassword)
    await modal.getByRole('button', { name: 'Change Password' }).click()

    // The modal closes and we stay authenticated (no LoginModal).
    await expect(modal).toBeHidden({ timeout: 5_000 })
    await expect(page.locator('.rail-avatar')).toBeVisible()
    await expect(page.locator('.modal-brand', { hasText: 'Peckboard' })).toHaveCount(0)

    // The old password is dead; the new one works.
    const oldRes = await request.post('/api/auth/login', {
      data: { username: u.username, password: u.password },
    })
    expect(oldRes.status()).toBe(401)
    const newRes = await request.post('/api/auth/login', {
      data: { username: u.username, password: newPassword },
    })
    expect(newRes.ok()).toBeTruthy()
  })

  test("admin can reset another user's password from the user-management page", async ({
    request,
    page,
  }) => {
    const { token: adminToken, auth: adminAuth } = await authenticateAdmin(request)
    const u = await createThrowawayUser(request, adminAuth, 'admin')
    const newPassword = 'admin-set-password-1234'

    // Sanity: the original password works before we touch it.
    const before = await request.post('/api/auth/login', {
      data: { username: u.username, password: u.password },
    })
    expect(before.ok()).toBeTruthy()

    await loadAs(page, adminToken, '/users')

    // The user row carries a "Reset password" button. Scope by username
    // so we don't accidentally hit the bootstrap admin row.
    const row = page.locator('.folder-row', { hasText: u.username })
    await expect(row).toBeVisible({ timeout: 5_000 })
    await row.getByRole('button', { name: 'Reset password' }).click()

    const modal = page.locator('.modal', { hasText: `Reset password for ${u.username}` })
    await expect(modal).toBeVisible()
    await modal.locator('#cp-new').fill(newPassword)
    await modal.locator('#cp-confirm').fill(newPassword)
    await modal.getByRole('button', { name: 'Reset Password' }).click()

    // Admin sees confirmation and dismisses; admin stays on the page.
    await expect(modal).toContainText('Password updated')
    await modal.getByRole('button', { name: 'Done' }).click()
    await expect(modal).toBeHidden()

    // Target's old password no longer works; the new one does.
    const oldRes = await request.post('/api/auth/login', {
      data: { username: u.username, password: u.password },
    })
    expect(oldRes.status()).toBe(401)
    const newRes = await request.post('/api/auth/login', {
      data: { username: u.username, password: newPassword },
    })
    expect(newRes.ok()).toBeTruthy()
  })
})
