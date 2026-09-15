import { test, expect, type APIRequestContext, type Page } from '../harness'

/**
 * UI e2e for the Plugins settings sub-page (Settings → Plugins).
 *
 * First-party providers and session-control ship as bundled crate plugins
 * (always enabled). Untrusted plugins remain WASM.
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

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

test('Plugins settings page lists bundled crate plugins; details modal shows permissions', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const token = await authenticate(request)
  await loadAppAt(page, token, '/plugins')
  const section = page.getByTestId('plugins-section')
  await expect(section).toBeVisible({ timeout: 10_000 })
  await expect(section.getByText('Bundled', { exact: true })).toBeVisible()

  const claudeRow = page.getByTestId('plugin-card-claude')
  await expect(claudeRow).toBeVisible()
  await expect(claudeRow).toContainText('Claude')

  const mockRow = page.getByTestId('plugin-card-mock')
  await expect(mockRow).toBeVisible()
  await expect(mockRow).toContainText('Mock')

  const sessionRow = page.getByTestId('plugin-card-session-control')
  await expect(sessionRow).toBeVisible()
  await expect(sessionRow).toContainText('Session Control')

  await claudeRow.getByTestId('plugin-open-claude').click()
  const claudeDetails = page.getByTestId('plugin-details-claude')
  await expect(claudeDetails).toBeVisible()
  await expect(claudeDetails).toContainText('Crate · always enabled')
  await expect(claudeDetails.locator('[data-permission="register_provider"]')).toBeVisible()
  await expect(claudeDetails.locator('[data-permission="spawn_process"]')).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(claudeDetails).toHaveCount(0)

  await mockRow.getByTestId('plugin-open-mock').click()
  const mockDetails = page.getByTestId('plugin-details-mock')
  await expect(mockDetails).toBeVisible()
  await expect(mockDetails.locator('[data-permission="register_provider"]')).toBeVisible()
  await expect(mockDetails.locator('[data-permission="spawn_process"]')).toHaveCount(0)
  await page.keyboard.press('Escape')

  await sessionRow.getByTestId('plugin-open-session-control').click()
  const sessionDetails = page.getByTestId('plugin-details-session-control')
  await expect(sessionDetails).toBeVisible()
  await expect(sessionDetails.locator('[data-permission="session_control"]')).toBeVisible()
  await expect(sessionDetails.locator('[data-permission="session_orchestrate"]')).toBeVisible()
})
