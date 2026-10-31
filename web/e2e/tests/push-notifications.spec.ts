import { test, expect, type APIRequestContext } from '../harness'

/**
 * Settings → Assistant → Notifications, end to end.
 *
 * Headless Chromium has no push service and reports notifications as
 * `denied` even after `grantPermissions`, so the permission and
 * `PushManager.subscribe` are stubbed (a fake FCM subscription with the
 * RFC 8291 example keys, so the server's key validation passes). The rest
 * is real: the service-worker registration and the subscribe / unsubscribe
 * round trips. Encryption + VAPID signing are covered by the Rust unit
 * tests in `src/service/push.rs`.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const P256DH =
  'BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4'
const AUTH = 'BTBZMqHH6r4Tts7J_aSIgg'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

test('Notifications: subscribe this browser, then turn it off', async ({ request, page }) => {
  const token = await authenticate(request)
  const endpoint = `https://fcm.googleapis.com/fcm/send/e2e-${Date.now()}`
  await page.addInitScript(
    ({ t, endpoint, p256dh, auth }) => {
      localStorage.setItem('peckboard_token', t)
      Object.defineProperty(Notification, 'permission', { get: () => 'granted' })
      Notification.requestPermission = async () => 'granted'
      let current: unknown = null
      const fake = () => ({
        endpoint,
        options: {},
        toJSON: () => ({ endpoint, keys: { p256dh, auth } }),
        unsubscribe: async () => {
          current = null
          return true
        },
      })
      PushManager.prototype.subscribe = async function () {
        current = fake()
        return current as PushSubscription
      }
      PushManager.prototype.getSubscription = async function () {
        return current as PushSubscription | null
      }
    },
    { t: token, endpoint, p256dh: P256DH, auth: AUTH },
  )

  await page.goto('/settings/assistant')
  const toggle = page.getByTestId('push-notifications-toggle')
  await expect(toggle).toBeVisible({ timeout: 10_000 })
  await expect(toggle).toBeEnabled()
  await expect(toggle).not.toBeChecked()

  const subscribed = page.waitForResponse(
    (r) => r.url().endsWith('/api/push/subscribe') && r.request().method() === 'POST',
  )
  // Controlled checkbox: it flips once the subscribe round trip lands.
  await toggle.click()
  const res = await subscribed
  expect(res.status()).toBe(201)
  const body = res.request().postDataJSON() as { endpoint: string; p256dh: string }
  expect(body.endpoint).toBe(endpoint)
  expect(body.p256dh).toBe(P256DH)
  await expect(toggle).toBeChecked()
  await expect(page.getByTestId('push-notifications-test')).toBeVisible()
  await expect(page.getByTestId('push-notifications-error')).toHaveCount(0)

  const unsubscribed = page.waitForResponse(
    (r) => r.url().endsWith('/api/push/subscribe') && r.request().method() === 'DELETE',
  )
  await toggle.click()
  expect((await unsubscribed).status()).toBe(204)
  await expect(toggle).not.toBeChecked()
  await expect(page.getByTestId('push-notifications-test')).toHaveCount(0)
})

test('Notifications: the server refuses endpoints that are not https push services', async ({
  request,
}) => {
  const token = await authenticate(request)
  const headers = { Authorization: `Bearer ${token}` }
  for (const endpoint of ['http://fcm.googleapis.com/fcm/send/x', 'https://127.0.0.1/push']) {
    const res = await request.post('/api/push/subscribe', {
      headers,
      data: { endpoint, p256dh: P256DH, auth_key: AUTH },
    })
    expect(res.status(), endpoint).toBe(400)
  }
})
