import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * UI e2e for the per-plugin settings modal opened from the Ollama card.
 *
 * Verifies:
 *
 * 1. The Ollama plugin card renders inside the Plugins modal, and
 *    clicking its "Settings" button opens a dedicated per-plugin
 *    modal layered on top.
 * 2. The settings form shows the typed fields the backend declared
 *    (base URL, default model, request timeout, additional headers).
 * 3. A change to base URL round-trips through the PUT endpoint and is
 *    reflected after reopening the modal on a fresh page load.
 * 4. The additional-headers key/value list lets the user add an entry
 *    that POSTs successfully; the saved value comes back masked on
 *    the next GET (the field is `secret_values: true`).
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

test('Ollama plugin renders its settings form and round-trips saves', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)
  await loadAppAt(page, token, '/plugins')

  await expect(page.getByTestId('plugins-modal')).toBeVisible({ timeout: 10_000 })
  const ollamaCard = page.getByTestId('plugin-card-ollama')
  await expect(ollamaCard).toBeVisible({ timeout: 10_000 })
  await expect(ollamaCard).toContainText('Ollama')

  // The card only shows a "Settings" button; the form lives inside its
  // own per-plugin modal so each plugin's config is isolated.
  await expect(ollamaCard.getByTestId('plugin-settings-ollama')).toHaveCount(0)
  await ollamaCard.getByTestId('plugin-settings-open-ollama').click()
  const settingsModal = page.getByTestId('plugin-settings-modal-ollama')
  await expect(settingsModal).toBeVisible({ timeout: 5_000 })
  const settings = settingsModal.getByTestId('plugin-settings-ollama')
  await expect(settings).toBeVisible()
  await expect(settings.locator('[data-field="base_url"]')).toBeVisible()
  await expect(settings.locator('[data-field="default_model"]')).toBeVisible()
  await expect(settings.locator('[data-field="request_timeout_secs"]')).toBeVisible()
  await expect(settings.locator('[data-field="additional_headers"]')).toBeVisible()

  // Edit the base URL.
  const baseUrlInput = settings.locator('[data-field="base_url"] input')
  await baseUrlInput.fill('http://ollama.test.local:11434')

  // Add one custom header.
  await settings.locator('.plugin-setting-kv-add').click()
  const kvRow = settings.locator('[data-field="additional_headers"] .plugin-setting-kv-row').first()
  await kvRow.locator('input').first().fill('X-Test-Header')
  await kvRow.locator('input').nth(1).fill('test-value-do-not-leak')

  await settings.locator('.plugin-settings-save').click()
  await expect(settings.locator('.plugin-settings-success')).toBeVisible({ timeout: 5_000 })

  // Reload the page and reopen the per-plugin modal to verify the base
  // URL persisted, and that the header KEY survives (it's not a secret)
  // but the VALUE is gone from the wire payload — the form input must
  // come back empty.
  await page.reload()
  const reopenedCard = page.getByTestId('plugin-card-ollama')
  await expect(reopenedCard).toBeVisible({ timeout: 10_000 })
  await reopenedCard.getByTestId('plugin-settings-open-ollama').click()
  const settingsAfter = page
    .getByTestId('plugin-settings-modal-ollama')
    .getByTestId('plugin-settings-ollama')
  await expect(settingsAfter.locator('[data-field="base_url"] input')).toHaveValue(
    'http://ollama.test.local:11434',
    { timeout: 10_000 },
  )

  const reloadedRow = settingsAfter
    .locator('[data-field="additional_headers"] .plugin-setting-kv-row')
    .first()
  await expect(reloadedRow.locator('input').first()).toHaveValue('X-Test-Header')
  await expect(reloadedRow.locator('input').nth(1)).toHaveValue('')

  // The hint text confirms a value is stored without exposing it.
  await expect(
    settingsAfter.locator('[data-field="additional_headers"] .plugin-setting-secret-set'),
  ).toBeVisible()
})
