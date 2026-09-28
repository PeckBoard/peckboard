import { test, expect, type APIRequestContext, type Page } from '../harness'

/**
 * Settings → Providers & Accounts → <provider> → Base Prompt.
 *
 * The editor is pre-filled with the provider's built-in default (Claude's
 * ask_user rules + the shared working style). Saving stores an override
 * that survives a reload and flags the block "Customized"; Reset to default
 * (behind a ConfirmDialog) drops it and restores the default text.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const CUSTOM = 'You are a terse e2e agent. Custom base prompt.'

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

test('provider base prompt: edit, persist across reload, reset to default', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const auth = { Authorization: `Bearer ${token}` }
  // Start from a known state: Claude visible, no override.
  await request.put('/api/settings/providers/claude', { headers: auth, data: { hidden: false } })
  const clear = await request.put('/api/settings/provider-prompts/claude', {
    headers: auth,
    data: { text: null },
  })
  expect(clear.ok()).toBeTruthy()

  await loadAppAt(page, token, '/settings/providers')
  const block = page.getByTestId('provider-prompt-claude')
  const input = page.getByTestId('provider-prompt-input-claude')
  const badge = page.getByTestId('provider-prompt-customized-claude')
  await expect(block).toBeVisible({ timeout: 10_000 })

  // Default: Claude-specific text plus the shared working style.
  await expect(input).toHaveValue(/mcp__peckboard__ask_user/)
  await expect(input).toHaveValue(/# Working style/)
  await expect(badge).toHaveCount(0)
  await expect(page.getByTestId('provider-prompt-reset-claude')).toBeDisabled()
  await expect(page.getByTestId('provider-prompt-save-claude')).toBeDisabled()

  await input.fill(CUSTOM)
  await page.getByTestId('provider-prompt-save-claude').click()
  await expect(badge).toBeVisible()

  const listed = await request.get('/api/settings/provider-prompts', { headers: auth })
  const entries = (await listed.json()) as { provider: string; override: string | null }[]
  expect(entries.find((e) => e.provider === 'claude')?.override).toBe(CUSTOM)

  await page.reload()
  await expect(input).toHaveValue(CUSTOM, { timeout: 10_000 })
  await expect(badge).toBeVisible()

  await page.getByTestId('provider-prompt-reset-claude').click()
  await page.getByTestId('confirm-dialog-confirm').click()
  await expect(badge).toHaveCount(0)
  await expect(input).toHaveValue(/mcp__peckboard__ask_user/)

  await page.reload()
  await expect(input).toHaveValue(/# Working style/, { timeout: 10_000 })
  await expect(badge).toHaveCount(0)
})
