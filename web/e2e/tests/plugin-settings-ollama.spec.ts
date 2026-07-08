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

  await expect(page.getByTestId('plugins-section')).toBeVisible({ timeout: 10_000 })
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
  await expect(settings.locator('[data-field="discover_models"]')).toBeVisible()
  await expect(settings.locator('[data-field="additional_headers"]')).toBeVisible()

  // Auto-discovery is a boolean that defaults to on. Turn it off so we
  // can prove the toggle round-trips through the PUT endpoint.
  const discoverToggle = settings.locator('[data-field="discover_models"] input[type="checkbox"]')
  await expect(discoverToggle).toBeChecked()
  await discoverToggle.uncheck()

  // Edit the base URL.
  const baseUrlInput = settings.locator('[data-field="base_url"] input')
  await baseUrlInput.fill('http://ollama.test.local:11434')

  // Add one custom header. Scope to the headers field — the
  // additional-models list reuses the same add-button class.
  const headersField = settings.locator('[data-field="additional_headers"]')
  await headersField.locator('.plugin-setting-kv-add').click()
  const kvRow = headersField.locator('.plugin-setting-kv-row').first()
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

  // The auto-discovery toggle we turned off stays off across a reload.
  await expect(
    settingsAfter.locator('[data-field="discover_models"] input[type="checkbox"]'),
  ).not.toBeChecked()

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

test('additional models registered in settings appear in the model catalog', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)
  await loadAppAt(page, token, '/plugins')

  await expect(page.getByTestId('plugins-section')).toBeVisible({ timeout: 10_000 })
  const ollamaCard = page.getByTestId('plugin-card-ollama')
  await expect(ollamaCard).toBeVisible({ timeout: 10_000 })
  await ollamaCard.getByTestId('plugin-settings-open-ollama').click()
  const settings = page
    .getByTestId('plugin-settings-modal-ollama')
    .getByTestId('plugin-settings-ollama')
  await expect(settings).toBeVisible({ timeout: 5_000 })

  // The additional-models list renders, distinct from the headers list.
  const modelsField = settings.locator('[data-field="additional_models"]')
  await expect(modelsField).toBeVisible()

  // Register a model whose name carries a tag colon — the field must not
  // reject it the way the header-name list would.
  await modelsField.locator('.plugin-setting-kv-add').click()
  await modelsField.locator('.plugin-setting-kv-row input').first().fill('llama3.1:8b')

  await settings.locator('.plugin-settings-save').click()
  await expect(settings.locator('.plugin-settings-success')).toBeVisible({ timeout: 5_000 })

  // The new model shows up in the catalog the picker reads, registered by
  // name under the ollama provider — live, without a server restart.
  const res = await request.get('/api/models', {
    headers: { Authorization: `Bearer ${token}` },
  })
  expect(res.ok(), `models fetch failed: ${await res.text()}`).toBeTruthy()
  const body = (await res.json()) as {
    models: { id: string; display_name: string }[]
  }
  const registered = body.models.find((m) => m.id === 'ollama:llama3.1:8b')
  expect(registered, 'additional model registered as ollama:llama3.1:8b').toBeTruthy()
  expect(registered?.display_name).toBe('llama3.1:8b (Ollama)')

  // Reopen on a fresh load: the entry persisted and round-trips into the
  // form (not a secret, so the value comes back verbatim).
  await page.reload()
  const reopened = page.getByTestId('plugin-card-ollama')
  await expect(reopened).toBeVisible({ timeout: 10_000 })
  await reopened.getByTestId('plugin-settings-open-ollama').click()
  const reloadedModels = page
    .getByTestId('plugin-settings-modal-ollama')
    .getByTestId('plugin-settings-ollama')
    .locator('[data-field="additional_models"]')
  await expect(reloadedModels.locator('.plugin-setting-kv-row input').first()).toHaveValue(
    'llama3.1:8b',
    { timeout: 10_000 },
  )
})
