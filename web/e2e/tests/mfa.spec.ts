/**
 * Two-factor auth: enable TOTP from Settings, second login factor,
 * recovery codes, disable, and admin reset.
 */

import { createHmac } from 'node:crypto'
import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

const ADMIN_USER = 'e2e-user'
const ADMIN_PASS = 'e2e-password-1234'

function base32Decode(input: string): Buffer {
  const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567'
  const clean = input.toUpperCase().replace(/[^A-Z2-7]/g, '')
  let bits = ''
  for (const c of clean) {
    const val = alphabet.indexOf(c)
    if (val < 0) continue
    bits += val.toString(2).padStart(5, '0')
  }
  const bytes: number[] = []
  for (let i = 0; i + 8 <= bits.length; i += 8) {
    bytes.push(parseInt(bits.slice(i, i + 8), 2))
  }
  return Buffer.from(bytes)
}

function totpNow(secretB32: string, nowMs = Date.now()): string {
  const key = base32Decode(secretB32)
  const timestep = Math.floor(nowMs / 1000 / 30)
  const buf = Buffer.alloc(8)
  buf.writeUInt32BE(Math.floor(timestep / 0x100000000), 0)
  buf.writeUInt32BE(timestep >>> 0, 4)
  const hmac = createHmac('sha1', key).update(buf).digest()
  const offset = hmac[hmac.length - 1] & 0x0f
  const bin =
    ((hmac[offset] & 0x7f) << 24) |
    ((hmac[offset + 1] & 0xff) << 16) |
    ((hmac[offset + 2] & 0xff) << 8) |
    (hmac[offset + 3] & 0xff)
  return (bin % 1_000_000).toString().padStart(6, '0')
}

let cachedAdminAuth: { token: string; auth: Record<string, string> } | null = null

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

async function createThrowawayUser(
  request: APIRequestContext,
  adminAuth: Record<string, string>,
  suffix: string,
): Promise<{ id: string; username: string; password: string }> {
  const username = `mfa-test-${suffix}-${Date.now()}`
  const password = 'orig-password-1234'
  const res = await request.post('/api/users', {
    headers: adminAuth,
    data: { username, password, role: 'user' },
  })
  expect(res.ok(), `create user failed: ${await res.text()}`).toBeTruthy()
  const user = (await res.json()) as { id: string }
  return { id: user.id, username, password }
}

async function loginAs(
  request: APIRequestContext,
  username: string,
  password: string,
): Promise<{ status: number; body: Record<string, unknown> }> {
  const res = await request.post('/api/auth/login', {
    data: { username, password },
  })
  const body = (await res.json()) as Record<string, unknown>
  return { status: res.status(), body }
}

async function loadAs(page: Page, token: string, route: string) {
  // Each call appends an init script; later tokens overwrite earlier ones on
  // navigation, so this can switch users mid-test.
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
  await expect(page.locator('.rail-avatar')).toBeVisible({ timeout: 10_000 })
}

async function signOut(page: Page) {
  await page.locator('.rail-avatar').click()
  await page.getByRole('menuitem', { name: 'Sign out' }).click()
  await expect(page.locator('.modal-brand', { hasText: 'Peckboard' })).toBeVisible({
    timeout: 10_000,
  })
}

async function enrollTotpFromSettings(
  page: Page,
  password: string,
): Promise<{ secret: string; recovery: string[] }> {
  await expect(page.getByTestId('mfa-section')).toBeVisible({ timeout: 10_000 })
  await page.getByTestId('mfa-enable').click()
  await page.getByTestId('mfa-setup-password').fill(password)
  await page.getByRole('button', { name: 'Continue' }).click()
  const secret = await page.getByTestId('mfa-secret').inputValue()
  expect(secret.length).toBeGreaterThan(8)
  await page.getByTestId('mfa-confirm-code').fill(totpNow(secret))
  await page.getByRole('button', { name: 'Confirm' }).click()
  await expect(page.getByTestId('mfa-recovery-codes')).toBeVisible({ timeout: 10_000 })
  const recovery = await page.getByTestId('mfa-recovery-codes').locator('code').allTextContents()
  expect(recovery).toHaveLength(10)
  await page.getByTestId('mfa-setup-done').click()
  await expect(page.getByTestId('mfa-disable')).toBeVisible()
  return { secret, recovery }
}

test.describe('two-factor auth', () => {
  test('enable TOTP from Settings, then sign in with a code', async ({ request, page }) => {
    const { auth: adminAuth } = await authenticateAdmin(request)
    const u = await createThrowawayUser(request, adminAuth, 'ui')
    const { status, body } = await loginAs(request, u.username, u.password)
    expect(status).toBe(200)
    await loadAs(page, body.token as string, '/settings/account')
    const { secret } = await enrollTotpFromSettings(page, u.password)

    await signOut(page)
    await page.locator('#login-username').fill(u.username)
    await page.locator('#login-password').fill(u.password)
    await page.getByRole('button', { name: 'Sign In' }).click()
    await expect(page.getByTestId('login-mfa-code')).toBeVisible({ timeout: 10_000 })
    await page.getByTestId('login-mfa-code').fill(totpNow(secret))
    await page.getByTestId('login-mfa-submit').click()
    await expect(page.locator('.rail-avatar')).toBeVisible({ timeout: 10_000 })
  })

  test('a recovery code signs in once, then is spent', async ({ request, page }) => {
    const { auth: adminAuth } = await authenticateAdmin(request)
    const u = await createThrowawayUser(request, adminAuth, 'rec')
    const { body } = await loginAs(request, u.username, u.password)
    await loadAs(page, body.token as string, '/settings/account')
    const { recovery } = await enrollTotpFromSettings(page, u.password)

    const first = await loginAs(request, u.username, u.password)
    expect(first.status).toBe(202)
    const challenge = first.body.challenge as string
    const ok = await request.post('/api/auth/mfa/verify', {
      data: { challenge, method: 'recovery', code: recovery[0] },
    })
    expect(ok.ok(), await ok.text()).toBeTruthy()

    const second = await loginAs(request, u.username, u.password)
    expect(second.status).toBe(202)
    const reused = await request.post('/api/auth/mfa/verify', {
      data: { challenge: second.body.challenge, method: 'recovery', code: recovery[0] },
    })
    expect(reused.status()).toBe(401)
  })

  test('disable 2FA returns login to password-only', async ({ request, page }) => {
    const { auth: adminAuth } = await authenticateAdmin(request)
    const u = await createThrowawayUser(request, adminAuth, 'off')
    const { body } = await loginAs(request, u.username, u.password)
    await loadAs(page, body.token as string, '/settings/account')
    const { secret } = await enrollTotpFromSettings(page, u.password)

    await page.getByTestId('mfa-disable').click()
    await page.getByTestId('mfa-action-password').fill(u.password)
    await page.getByTestId('mfa-action-code').fill(totpNow(secret))
    await page.getByTestId('mfa-action-confirm').click()
    await expect(page.getByTestId('mfa-enable')).toBeVisible({ timeout: 10_000 })
    await expect(page.locator('.rail-avatar')).toBeVisible()

    const after = await loginAs(request, u.username, u.password)
    expect(after.status).toBe(200)
    expect(after.body.token).toBeTruthy()
  })

  test('admin Reset 2FA lets the user sign in with password only', async ({ request, page }) => {
    const { token: adminToken, auth: adminAuth } = await authenticateAdmin(request)
    const u = await createThrowawayUser(request, adminAuth, 'adm')
    const { body } = await loginAs(request, u.username, u.password)
    await loadAs(page, body.token as string, '/settings/account')
    await enrollTotpFromSettings(page, u.password)

    await loadAs(page, adminToken, '/settings/users')
    await expect(page.getByTestId(`user-reset-2fa-${u.username}`)).toBeVisible({ timeout: 10_000 })
    await page.getByTestId(`user-reset-2fa-${u.username}`).click()
    await page
      .getByTestId('user-reset-2fa-confirm')
      .getByRole('button', { name: 'Reset 2FA' })
      .click()
    await expect(page.getByTestId(`user-reset-2fa-${u.username}`)).toHaveCount(0)

    const after = await loginAs(request, u.username, u.password)
    expect(after.status).toBe(200)
    expect(after.body.token).toBeTruthy()
  })
})
