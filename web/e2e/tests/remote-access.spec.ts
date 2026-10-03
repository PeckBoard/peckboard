import { test, expect, type APIRequestContext, type Page } from '../harness'

/**
 * UI e2e for Settings → Remote Access (relay pairing).
 *
 * Pair a device → the one-time link + QR + `peckboard-connect` command are
 * shown → the device appears in the list → revoke it through the shared
 * confirm. Remote access stays OFF for the whole test (the default), so the
 * server never registers with a relay and nothing here touches
 * relay.peckboard.com; the API is also checked to never hand the secret
 * back after creation.
 */

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

async function loadApp(page: Page, token: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/')
  await expect(page.locator('.rail-brand')).toBeVisible({ timeout: 10_000 })
}

test('pair a device, see its link once, then revoke it', async ({ request, page }) => {
  const token = await authenticate(request)
  await loadApp(page, token)

  await page.locator('.rail-avatar').click()
  await page.locator('.user-menu-dropdown').getByRole('menuitem', { name: 'Settings' }).click()
  const settings = page.getByTestId('settings-page')
  await settings.getByTestId('settings-nav-remote-access').click()
  const section = settings.getByTestId('remote-access-section')
  await expect(section).toBeVisible()
  await expect(section.getByTestId('remote-devices-empty')).toBeVisible()
  // Off by default; the relay host defaults to the public relay.
  await expect(section.getByTestId('remote-access-off')).toHaveClass(/active/)
  await expect(section.getByTestId('remote-relay-host')).toHaveValue('relay.peckboard.com')

  // ── Pair ───────────────────────────────────────────────────────────
  await section.getByTestId('remote-pair-device').click()
  const modal = page.getByTestId('remote-pair-modal')
  await expect(modal.getByTestId('remote-pair-submit')).toBeDisabled()
  await modal.getByTestId('remote-pair-name').fill('e2e-laptop')
  await modal.getByTestId('remote-pair-submit').click()

  // ── The link, QR code and connect command, shown once ──────────────
  const linkModal = page.getByTestId('remote-pair-link-modal')
  await expect(linkModal).toBeVisible()
  const link = linkModal.getByTestId('remote-pair-link')
  await expect(link).toHaveValue(
    /^peckboard:\/\/pair\/[A-Za-z0-9_-]{43}\?relay=relay\.peckboard\.com$/,
  )
  const linkValue = await link.inputValue()
  await expect(linkModal.getByTestId('remote-pair-qr').locator('svg')).toBeVisible()
  await expect(linkModal.getByTestId('remote-pair-command')).toHaveText(
    `peckboard-connect ${linkValue}`,
  )
  await linkModal.getByTestId('remote-pair-done').click()
  await expect(linkModal).toBeHidden()

  // ── Listed, and the secret is never served again ───────────────────
  await expect(section.getByTestId('remote-device-row-e2e-laptop')).toBeVisible()
  await expect(section.getByTestId('remote-device-state-e2e-laptop')).toHaveText('off')
  const secretPart = linkValue.slice('peckboard://pair/'.length).split('?')[0]
  const listRes = await request.get('/api/remote-access', {
    headers: { Authorization: `Bearer ${token}` },
  })
  expect(listRes.ok()).toBeTruthy()
  expect(await listRes.text()).not.toContain(secretPart)

  // ── Revoke, via the row's 3-dot menu and the shared confirm ────────
  const row = section.locator('.list-view-row').filter({ hasText: 'e2e-laptop' })
  await row.locator('.list-view-menu').click()
  await page.getByRole('menuitem', { name: 'Revoke' }).click()
  const confirm = page.getByTestId('remote-device-revoke-confirm')
  await expect(confirm).toContainText('pairing secret is destroyed')
  await confirm.getByTestId('confirm-dialog-confirm').click()
  await expect(section.getByTestId('remote-device-row-e2e-laptop')).toHaveCount(0)
  await expect(section.getByTestId('remote-devices-empty')).toBeVisible()
})
