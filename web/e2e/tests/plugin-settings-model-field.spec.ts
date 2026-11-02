import { test, expect, type APIRequestContext, type Page } from '../harness'

/**
 * UI e2e for the `type: "model"` plugin settings field: a searchable
 * combobox over `/api/models` with free-text fallback.
 *
 * Like `plugin-settings-wasm.spec.ts`, the plugin catalog and settings
 * routes are mocked (no wasm32 toolchain in CI); `/api/models` is mocked
 * too so the option list — including an account-scoped `@account` id — is
 * deterministic. Backend parsing/validation of the type is covered by the
 * `settings.rs` / `hooks.rs` unit tests.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const PLUGIN = 'story-writer'

const SCHEMA = {
  fields: [
    {
      key: 'writer_model',
      title: 'Writer model',
      description: 'Model that drafts chapters.',
      type: 'model',
    },
    { key: 'reviewer_model', title: 'Reviewer model', type: 'model' },
  ],
}

const MODELS = [
  { id: 'claude:claude-opus-5-5', display_name: 'Opus 5.5' },
  { id: 'claude:claude-sonnet-5', display_name: 'Sonnet 5' },
  { id: 'claude:claude-sonnet-5@acc_work', display_name: 'Sonnet 5 (work)' },
]

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

test('model settings field searches the catalog, saves picks and free-text ids', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)

  await page.route('**/api/models', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        models: MODELS,
        providers: [{ id: 'claude', display_name: 'Claude', models: MODELS }],
      }),
    })
  })

  await page.route('**/api/plugins', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        plugins: [],
        ui_panels: [],
        wasm_plugins: [
          {
            name: PLUGIN,
            description: 'Writes stories.',
            version: '0.1.0',
            repository: 'https://github.com/PeckBoard/story-writer',
            hooks: ['mcp.tool.invoke'],
            permissions: ['provide_mcp_tools'],
            status: 'approved',
            error: null,
            stats: {
              calls: 0,
              errors: 0,
              rebuilds: 0,
              busy_ms: 0,
              last_call_ms: null,
              last_call_at: null,
              wasm_bytes: 1024,
            },
            settings_schema: SCHEMA,
          },
        ],
      }),
    })
  })

  // Stored values: writer_model holds an id the catalog doesn't list (it
  // was saved while the field was still `type: "string"`).
  const stored: Record<string, string | null> = {
    writer_model: 'ollama:offline-model',
    reviewer_model: 'claude:claude-opus-5-5',
  }
  const wire = () => ({
    plugin_id: PLUGIN,
    schema: SCHEMA,
    settings: Object.entries(stored).map(([key, value]) => ({
      key,
      value,
      has_value: value !== null,
      masked: false,
    })),
  })
  const putUpdates: Record<string, unknown>[] = []
  await page.route(`**/api/plugins/${PLUGIN}/settings`, async (route) => {
    if (route.request().method() === 'PUT') {
      const { updates } = route.request().postDataJSON() as { updates: Record<string, string> }
      putUpdates.push(updates)
      Object.assign(stored, updates)
    }
    await route.fulfill({ contentType: 'application/json', body: JSON.stringify(wire()) })
  })

  await loadAppAt(page, token, '/plugin-settings')
  await page.getByTestId(`wasm-plugin-open-${PLUGIN}`).click()
  const form = page.getByTestId(`plugin-settings-${PLUGIN}`)
  const writer = form.getByTestId('plugin-setting-model-writer_model')
  const reviewer = form.getByTestId('plugin-setting-model-reviewer_model')

  // A stored id missing from the catalog is kept and flagged, not blanked;
  // a known one shows its display name.
  await expect(writer).toHaveValue('ollama:offline-model')
  await expect(form.getByTestId('plugin-setting-model-writer_model-not-in-catalog')).toBeVisible()
  await expect(reviewer).toHaveValue('claude:claude-opus-5-5')
  await expect(form.getByTestId('plugin-setting-model-reviewer_model-name')).toHaveText('Opus 5.5')

  // Focus lists the whole catalog (not one filtered to the stored id)…
  await writer.click()
  const options = page.getByRole('listbox', { name: 'Models' }).getByRole('option')
  await expect(options).toHaveCount(3)
  // …and typing narrows it, matching id and display name.
  await writer.fill('sonnet')
  await expect(options).toHaveCount(2)
  await expect(page.getByTestId('model-option-claude:claude-opus-5-5')).toHaveCount(0)
  await page.screenshot({ path: 'e2e/test-results/plugin-settings-model-field.png' })

  // Picking an account-scoped entry saves it immediately.
  await page.getByTestId('model-option-claude:claude-sonnet-5@acc_work').click()
  await expect(form.locator('.plugin-settings-success')).toBeVisible({ timeout: 5_000 })
  await expect(writer).toHaveValue('claude:claude-sonnet-5@acc_work')
  await expect(form.getByTestId('plugin-setting-model-writer_model-name')).toHaveText(
    'Sonnet 5 (work)',
  )
  await expect(form.getByTestId('plugin-setting-model-writer_model-not-in-catalog')).toHaveCount(0)

  // Free-text fallback: an id the catalog doesn't know saves on Enter.
  await writer.fill('grok:grok-9-preview')
  await expect(page.getByRole('listbox', { name: 'Models' })).toContainText('No catalog match')
  await writer.press('Enter')
  await expect.poll(() => putUpdates.length).toBe(2)
  await expect(writer).toHaveValue('grok:grok-9-preview')
  await expect(form.getByTestId('plugin-setting-model-writer_model-not-in-catalog')).toBeVisible()

  expect(putUpdates).toEqual([
    { writer_model: 'claude:claude-sonnet-5@acc_work' },
    { writer_model: 'grok:grok-9-preview' },
  ])
})
