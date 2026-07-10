import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * Provider visibility toggles on Settings → Providers.
 *
 * Providers are visible by default. Toggling one off hides its settings
 * section, removes its models from /api/models (and thus all model
 * pickers including the pre-hatcher dropdown), and persists across
 * reload. Uses ollama — always registered, has a dedicated settings
 * section with data-testid="ollama-settings-section".
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

async function navigateToProviders(page: Page) {
  await expect(page.locator('.rail-brand')).toBeVisible({ timeout: 10_000 })
  await page.locator('.rail-avatar').click()
  const menu = page.locator('.user-menu-dropdown')
  await expect(menu).toBeVisible()
  await menu.getByRole('menuitem', { name: 'Settings' }).click()
  const settingsPage = page.getByTestId('settings-page')
  await expect(settingsPage).toBeVisible()
  await settingsPage.getByTestId('settings-nav-providers').click()
  return settingsPage
}

type ModelsResponse = {
  providers: Array<{ id: string; models: Array<{ id: string }> }>
  models: Array<{ id: string }>
}

async function fetchModels(
  request: APIRequestContext,
  authHeader: Record<string, string>,
): Promise<ModelsResponse> {
  const res = await request.get('/api/models', { headers: authHeader })
  expect(res.ok()).toBeTruthy()
  return (await res.json()) as ModelsResponse
}

test('toggling a provider off hides its section and models; toggle on restores; persists across reload', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL).toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  await loadAppAt(page, token, '/')

  const settingsPage = await navigateToProviders(page)

  // All toggles checked (visible) by default.
  const toggle = settingsPage.getByTestId('provider-toggle-ollama')
  await expect(toggle).toBeVisible()
  await expect(toggle).toBeChecked()
  await expect(settingsPage.getByTestId('ollama-settings-section')).toBeVisible()

  // --- Toggle OFF ---
  await toggle.click()
  await expect(toggle).not.toBeChecked({ timeout: 5_000 })
  await expect(settingsPage.getByTestId('ollama-settings-section')).toBeHidden()

  // /api/models should exclude ollama.
  const modelsHidden = await fetchModels(request, authHeader)
  expect(modelsHidden.providers.find((p) => p.id === 'ollama')).toBeUndefined()
  expect(modelsHidden.models.some((m) => m.id.startsWith('ollama:'))).toBe(false)

  // Pre-hatcher dropdown on Chat sub-page: no Ollama optgroup.
  await settingsPage.getByRole('button', { name: 'Back' }).click()
  await settingsPage.getByTestId('settings-nav-chat').click()
  const preHatchSelect = settingsPage.getByTestId('prehatch-model-select')
  await expect(preHatchSelect).toBeVisible()
  await expect(preHatchSelect.locator('optgroup[label="Ollama"]')).toHaveCount(0)

  // --- Toggle back ON ---
  await settingsPage.getByRole('button', { name: 'Back' }).click()
  await settingsPage.getByTestId('settings-nav-providers').click()
  const toggleOn = settingsPage.getByTestId('provider-toggle-ollama')
  await toggleOn.click()
  await expect(toggleOn).toBeChecked({ timeout: 5_000 })
  await expect(settingsPage.getByTestId('ollama-settings-section')).toBeVisible()

  // /api/models should include ollama again.
  const modelsRestored = await fetchModels(request, authHeader)
  expect(modelsRestored.providers.find((p) => p.id === 'ollama')).toBeTruthy()

  // Pre-hatcher dropdown: Ollama optgroup restored.
  await settingsPage.getByRole('button', { name: 'Back' }).click()
  await settingsPage.getByTestId('settings-nav-chat').click()
  await expect(preHatchSelect).toBeVisible()
  await expect(preHatchSelect.locator('optgroup[label="Ollama"]')).toHaveCount(1)

  // --- Persist across reload ---
  await settingsPage.getByRole('button', { name: 'Back' }).click()
  await settingsPage.getByTestId('settings-nav-providers').click()
  const togglePersist = settingsPage.getByTestId('provider-toggle-ollama')
  await togglePersist.click()
  await expect(togglePersist).not.toBeChecked({ timeout: 5_000 })
  await expect(settingsPage.getByTestId('ollama-settings-section')).toBeHidden()

  await page.reload()
  const sp2 = await navigateToProviders(page)
  const toggleAfterReload = sp2.getByTestId('provider-toggle-ollama')
  await expect(toggleAfterReload).not.toBeChecked()
  await expect(sp2.getByTestId('ollama-settings-section')).toBeHidden()

  // Restore for other tests.
  await toggleAfterReload.click()
  await expect(toggleAfterReload).toBeChecked({ timeout: 5_000 })
})

test('hiding a provider via API removes its models from /api/models', async ({ request }) => {
  const { authHeader } = await authenticate(request)

  const before = await fetchModels(request, authHeader)
  expect(before.providers.find((p) => p.id === 'mock')).toBeTruthy()

  const hideRes = await request.put('/api/settings/providers/mock', {
    headers: { ...authHeader, 'Content-Type': 'application/json' },
    data: { hidden: true },
  })
  expect(hideRes.ok(), `hide failed: ${await hideRes.text()}`).toBeTruthy()

  const after = await fetchModels(request, authHeader)
  expect(after.providers.find((p) => p.id === 'mock')).toBeUndefined()
  expect(after.models.some((m) => m.id.startsWith('mock:'))).toBe(false)

  const restoreRes = await request.put('/api/settings/providers/mock', {
    headers: { ...authHeader, 'Content-Type': 'application/json' },
    data: { hidden: false },
  })
  expect(restoreRes.ok()).toBeTruthy()

  const restored = await fetchModels(request, authHeader)
  expect(restored.providers.find((p) => p.id === 'mock')).toBeTruthy()
})
