import { test, expect, type APIRequestContext } from '../harness'

/**
 * "What's new" card after an upgrade (components/WhatsNew.tsx) and the
 * phone-pairing QR endpoint the PhonePairingCard reads.
 *
 * - First-ever load records the running version silently: no card.
 * - With an older last-seen version, the card lists the highlights of the
 *   versions in between; dismissing it records the running version, so a
 *   reload shows nothing.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function login(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  const auth = { Authorization: `Bearer ${token}` }
  const me = (await (await request.get('/api/auth/me', { headers: auth })).json()) as {
    id: string
  }
  const cfg = (await (await request.get('/api/config', { headers: auth })).json()) as {
    version: string
  }
  return { token, auth, userId: me.id, version: cfg.version }
}

test('first load records the version silently; an upgrade shows the card once', async ({
  request,
  page,
}) => {
  const { token, userId, version } = await login(request)
  expect(version).toMatch(/^\d+\.\d+\.\d+/)
  const key = `peckboard.whatsNew.lastSeen.${userId}`

  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto('/')

  // Fresh: nothing shown, the running version is recorded.
  await expect.poll(() => page.evaluate((k) => localStorage.getItem(k), key)).toBe(version)
  await expect(page.getByTestId('whats-new')).toHaveCount(0)

  // Pretend the user last saw a much older release, then reload.
  await page.evaluate((k) => localStorage.setItem(k, '0.0.1'), key)
  await page.reload()

  const card = page.getByTestId('whats-new')
  await expect(card).toBeVisible()
  await expect(card).toContainText(`What’s new in ${version}`)
  // An entry between 0.0.1 and the running version is listed.
  await expect(card.getByTestId('whats-new-version-0.1.156')).toBeVisible()
  await expect(card).toContainText('Skills')

  await card.getByTestId('whats-new-dismiss').click()
  await expect(card).toHaveCount(0)
  expect(await page.evaluate((k) => localStorage.getItem(k), key)).toBe(version)

  await page.reload()
  await expect(page.locator('main').first()).toBeVisible()
  await expect(page.getByTestId('whats-new')).toHaveCount(0)
})

test('pairing QR encodes an HTTPS page origin', async ({ request }) => {
  const { auth } = await login(request)
  const res = await request.get(
    `/api/pairing/qr?origin=${encodeURIComponent('https://box.example:8443')}`,
    { headers: auth },
  )
  expect(res.ok()).toBeTruthy()
  const body = (await res.json()) as {
    url: string | null
    qr_svg: string | null
    reachable: boolean
  }
  expect(body.reachable).toBe(true)
  expect(body.url).toBe('https://box.example:8443')
  expect(body.qr_svg).toContain('<svg')

  const anon = await request.get('/api/pairing/qr')
  expect(anon.status()).toBe(401)
})
