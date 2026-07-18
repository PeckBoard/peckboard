import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Preset prompts in the New Session modal (NewSessionModal.tsx):
 *
 *  - the Preset dropdown defaults to "None — start empty"; the browser
 *    bug-hunt preset is hidden unless the playwright-video WASM plugin is
 *    installed + approved (mocked via /api/plugins here);
 *  - "Research a topic" reveals a required Topic field, auto-names the
 *    session, and the built prompt lands as the session's first `user`
 *    event (create-then-message, like the MCP install flow);
 *  - checking Temporary hides the Name field and the session is
 *    auto-named.
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

async function createFolder(request: APIRequestContext, authHeader: Record<string, string>) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-preset-'))
  const res = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-preset-${Date.now()}`, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function openNewSessionModal(page: Page, token: string) {
  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto('/')
  await expect(page.locator('.rail-avatar')).toBeVisible({ timeout: 10_000 })
  await page.locator('.tab-new').click()
  await expect(page.getByTestId('new-session-preset')).toBeVisible()
}

/** Pick a mock model in the modal so the preset's first message dispatches
 *  against the deterministic mock provider. */
async function pickHappyPathModel(page: Page) {
  await page.getByTestId('new-session-model').click()
  await page.getByTestId('new-session-model-search').fill('happy')
  await page.getByRole('option', { name: 'Mock: happy path' }).click()
}

test('research preset: topic field, auto-name, prompt sent as first message', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  await createFolder(request, authHeader)
  await openNewSessionModal(page, token)

  const presetSelect = page.getByTestId('new-session-preset')
  // Default is no preset, and without the playwright-video plugin the
  // bug-hunt preset is not offered.
  await expect(presetSelect).toHaveValue('')
  await expect(presetSelect.locator('option', { hasText: 'Hunt for bugs (browser)' })).toHaveCount(
    0,
  )
  await expect(page.getByTestId('new-session-preset-topic')).toHaveCount(0)

  await presetSelect.selectOption('research')
  const topicInput = page.getByTestId('new-session-preset-topic')
  await expect(topicInput).toBeVisible()

  // Topic is required: create stays disabled until it's filled (the name
  // may stay empty — the preset auto-names the session).
  await expect(page.getByRole('button', { name: 'Create Session' })).toBeDisabled()
  const topic = `quantum-error-correction-${Date.now()}`
  await topicInput.fill(topic)
  await expect(page.getByRole('button', { name: 'Create Session' })).toBeEnabled()

  await pickHappyPathModel(page)
  await page.screenshot({ path: 'test-results/preset-research-modal.png' })
  await page.getByRole('button', { name: 'Create Session' }).click()

  // Session auto-named from the preset + topic.
  const expectedName = `Research a topic: ${topic}`
  const chip = page.locator('.tab-wrap', { hasText: expectedName })
  await expect(chip).toBeVisible()

  const sessions = (await (
    await request.get('/api/sessions?limit=100', { headers: authHeader })
  ).json()) as { items: Array<{ id: string; name: string }> }
  const ours = sessions.items.find((s) => s.name === expectedName)
  expect(ours, 'preset-named session should exist').toBeTruthy()

  // The built research prompt (carrying the topic) was sent as the first
  // user message.
  await expect
    .poll(
      async () => {
        const res = await request.get(`/api/sessions/${ours!.id}/events?limit=50`, {
          headers: authHeader,
        })
        const events = (await res.json()) as Array<{ kind: string; data: { text?: string } }>
        return events.find((e) => e.kind === 'user')?.data.text ?? null
      },
      { message: 'first user event should carry the research prompt' },
    )
    .toContain(topic)
  await page.screenshot({ path: 'test-results/preset-research-session.png' })
})

test('bug-hunt preset is offered once the playwright-video plugin is active', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  await createFolder(request, authHeader)

  // Mock the plugin catalog: playwright-video installed + approved. Shape
  // mirrors GET /api/plugins (routes/plugins.rs) with empty contributions
  // so App's own consumers stay happy.
  await page.route('**/api/plugins', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        plugins: [],
        ui_panels: [],
        sidebar_items: [],
        project_items: [],
        session_items: [],
        wasm_plugins: [
          {
            name: 'playwright-video',
            description: 'replay',
            version: '0.3.2',
            repository: 'https://github.com/PeckBoard/playwright-video',
            hooks: [],
            permissions: [],
            status: 'approved',
          },
        ],
      }),
    }),
  )

  await openNewSessionModal(page, token)
  const presetSelect = page.getByTestId('new-session-preset')
  await expect(presetSelect.locator('option', { hasText: 'Hunt for bugs (browser)' })).toHaveCount(
    1,
  )
  await presetSelect.selectOption('bug-hunt')
  // No extra field for this preset, and no name needed — the preset
  // auto-names the session.
  await expect(page.getByTestId('new-session-preset-topic')).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Create Session' })).toBeEnabled()
  await page.screenshot({ path: 'test-results/preset-bug-hunt-option.png' })
})

test('temporary checkbox hides the Name field and auto-names the session', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  await createFolder(request, authHeader)
  await openNewSessionModal(page, token)

  const nameInput = page.getByPlaceholder('My session')
  await expect(nameInput).toBeVisible()

  await page.getByTestId('new-session-temp').check()
  await expect(nameInput).toHaveCount(0)
  await page.screenshot({ path: 'test-results/preset-temp-no-name.png' })

  // Toggling back restores the field…
  await page.getByTestId('new-session-temp').uncheck()
  await expect(page.getByPlaceholder('My session')).toBeVisible()
  await page.getByTestId('new-session-temp').check()

  // …and a temp session needs no name at all (no preset → no dispatch).
  // Resolve the created session by diffing the tab rows across create —
  // the auto name ("Temp session") is not unique across specs.
  const listTabs = async () =>
    (await (await request.get('/api/me/tabs', { headers: authHeader })).json()) as {
      item_id: string
      name: string
      is_temp: boolean
    }[]
  const before = new Set((await listTabs()).map((t) => t.item_id))
  await expect(page.getByRole('button', { name: 'Create Session' })).toBeEnabled()
  await page.getByRole('button', { name: 'Create Session' }).click()

  let created: { item_id: string; name: string; is_temp: boolean } | undefined
  await expect
    .poll(async () => {
      created = (await listTabs()).find((t) => !before.has(t.item_id))
      return created?.name ?? null
    })
    .toBe('Temp session')
  expect(created?.is_temp).toBe(true)

  const chip = page.locator(`.tab-wrap[data-tab-id="session:${created!.item_id}"]`)
  await expect(chip).toBeVisible()
  await expect(chip.locator('.tab-icon-temp-session')).toBeVisible()
  await page.screenshot({ path: 'test-results/preset-temp-created.png' })

  // Clean up: closing the chip deletes the temp session, so later specs
  // never meet a stray "Temp session" tab.
  await chip.locator('.tab-close').click()
  await expect(chip).toHaveCount(0)
})
