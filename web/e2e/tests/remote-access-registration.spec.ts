import { test, expect, type APIRequestContext, type BrowserContext, type Page } from '../harness'

/**
 * UI e2e for relay registration in Settings → Remote Access.
 *
 * The box's remote-access API is mocked at the browser edge (`page.route`),
 * so the server never turns remote access on and nothing here contacts a
 * relay: the mocked overview reports each registration state the UI must
 * handle, the mocked refresh endpoint flips to registered after a few
 * polls, and the relay's registration page is a stub served to the popup.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const RELAY = 'relay.test'
const KEY = 'MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY'
const REGISTER_URL = `https://${RELAY}/register#${KEY}`

interface Registration {
  supported: boolean
  registered: boolean | null
  gated: boolean | null
  url: string
}

interface MockState {
  enabled: boolean
  registration: Registration
  refreshes: number
  /** The refresh call (1-based) from which the relay reports registered. */
  registerOnRefresh: number
}

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

async function mockRemoteAccess(page: Page, context: BrowserContext, state: MockState) {
  await page.route('**/api/remote-access', async (route) => {
    if (route.request().method() === 'PUT') {
      const body = route.request().postDataJSON() as { enabled?: boolean }
      if (body.enabled !== undefined) state.enabled = body.enabled
      await route.fulfill({
        json: {
          enabled: state.enabled,
          relay_host: RELAY,
          udp_port_base: null,
          udp_port_count: 10,
          public_address: '',
        },
      })
      return
    }
    const real = await route.fetch()
    const json = (await real.json()) as Record<string, unknown>
    await route.fulfill({
      json: {
        ...json,
        enabled: state.enabled,
        relay_host: RELAY,
        registration: state.registration,
      },
    })
  })
  await page.route('**/api/remote-access/registration/refresh', async (route) => {
    state.refreshes += 1
    if (state.refreshes >= state.registerOnRefresh) {
      state.registration = { ...state.registration, registered: true }
    }
    await route.fulfill({ json: state.registration })
  })
  // The popup lands here instead of on a real relay.
  await context.route(`https://${RELAY}/**`, (route) =>
    route.fulfill({
      contentType: 'text/html',
      body: '<title>Register your PeckBoard box</title><h1>stub</h1>',
    }),
  )
}

async function openRemoteAccess(page: Page, token: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/')
  await expect(page.locator('.rail-brand')).toBeVisible({ timeout: 10_000 })
  await page.locator('.rail-avatar').click()
  await page.locator('.user-menu-dropdown').getByRole('menuitem', { name: 'Settings' }).click()
  const settings = page.getByTestId('settings-page')
  await settings.getByTestId('settings-nav-remote-access').click()
  const section = settings.getByTestId('remote-access-section')
  await expect(section).toBeVisible()
  return section
}

test('turning remote access on opens the relay registration page and waits until registered', async ({
  request,
  page,
  context,
}) => {
  const token = await authenticate(request)
  const state: MockState = {
    enabled: false,
    registration: { supported: true, registered: false, gated: true, url: REGISTER_URL },
    refreshes: 0,
    registerOnRefresh: Number.MAX_SAFE_INTEGER,
  }
  await mockRemoteAccess(page, context, state)
  const section = await openRemoteAccess(page, token)
  await expect(section.getByTestId('remote-access-off')).toHaveClass(/active/)
  // Off: nothing about registration is shown.
  await expect(section.getByTestId('remote-registration')).toHaveCount(0)

  // ── On: the status is fetched while the confirm is up, and the confirm
  //    click itself opens the relay's page (popup blockers allow that). ──
  await section.getByTestId('remote-access-on').click()
  const confirm = page.getByTestId('remote-access-enable-confirm')
  await expect(confirm).toContainText('registration page opens in a new tab')
  await expect.poll(() => state.refreshes).toBeGreaterThan(0)
  const [popup] = await Promise.all([
    context.waitForEvent('page'),
    confirm.getByTestId('confirm-dialog-confirm').click(),
  ])
  await popup.waitForURL((u) => u.href === REGISTER_URL, { timeout: 10_000 })
  await popup.close()

  await expect(section.getByTestId('remote-access-on')).toHaveClass(/active/)
  const status = section.getByTestId('remote-registration-status')
  await expect(status).toContainText('Waiting for registration')
  await expect(section.getByTestId('remote-registration-open')).toHaveText('Open again')

  // ── The admin registers: the box polls, the line flips, the button goes. ──
  state.registerOnRefresh = state.refreshes + 2
  await expect(status).toHaveText(`Registered with ${RELAY}`, { timeout: 20_000 })
  await expect(section.getByTestId('remote-registration-open')).toHaveCount(0)
})

test('an unregistered box that is already on offers Register; an old relay shows nothing', async ({
  request,
  page,
  context,
}) => {
  const token = await authenticate(request)
  const state: MockState = {
    enabled: true,
    registration: { supported: true, registered: false, gated: false, url: REGISTER_URL },
    refreshes: 0,
    registerOnRefresh: Number.MAX_SAFE_INTEGER,
  }
  await mockRemoteAccess(page, context, state)
  const section = await openRemoteAccess(page, token)
  const status = section.getByTestId('remote-registration-status')
  // Gate off: not scary — everything still works.
  await expect(status).toContainText(`Not registered with ${RELAY}`)
  await expect(status).not.toContainText('unavailable')
  const [popup] = await Promise.all([
    context.waitForEvent('page'),
    section.getByTestId('remote-registration-open').click(),
  ])
  await popup.waitForURL((u) => u.href === REGISTER_URL, { timeout: 10_000 })
  await popup.close()
  await expect(status).toContainText('Waiting for registration')

  // Gate on: the relayed fallback is what's missing, and only that.
  state.registration = { ...state.registration, gated: true }
  await page.reload()
  const again = await openRemoteAccess(page, token)
  await expect(again.getByTestId('remote-registration-status')).toContainText(
    'Not registered — relayed fallback unavailable',
  )
  await expect(again.getByTestId('remote-registration-open')).toHaveText('Register')

  // A relay that predates relay registration: no registration UI at all.
  state.registration = { supported: false, registered: null, gated: null, url: '' }
  await expect(again.getByTestId('remote-registration')).toHaveCount(0, { timeout: 15_000 })
  await expect(again.getByTestId('remote-access-on')).toHaveClass(/active/)
})
