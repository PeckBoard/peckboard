import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Effort levels are defined per provider and served by `/api/models`. The
 * effort picker loads the chosen model's provider's levels once a model is
 * selected. This proves:
 *  - `/api/models` carries `effort_levels` per provider, with Claude's full
 *    ladder including the added `xhigh` (Extra high) and `max`.
 *  - The New Repeating Task form's Effort dropdown offers those levels and a
 *    provider-effort value round-trips onto the spawned task.
 *
 * Uses the mock provider (which mirrors the standard ladder) so the run is
 * deterministic — see `repeating-tasks-ui.spec.ts` for the base pattern.
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

async function loginUi(page: import('@playwright/test').Page, baseURL: string) {
  await page.goto(baseURL)
  await page.getByLabel('Username').fill(E2E_USER)
  await page.getByLabel('Password').fill(E2E_PASS)
  await page.getByRole('button', { name: /sign in/i }).click()
}

test('/api/models exposes per-provider effort levels including xhigh and max', async ({
  request,
}) => {
  const { authHeader } = await authenticate(request)
  const res = await request.get('/api/models', { headers: authHeader })
  expect(res.ok(), `models fetch failed: ${await res.text()}`).toBeTruthy()
  const data = (await res.json()) as {
    providers: Array<{ id: string; effort_levels?: Array<{ id: string; label: string }> }>
  }

  // Every provider carries an effort_levels array (empty is valid).
  for (const p of data.providers) {
    expect(Array.isArray(p.effort_levels), `provider ${p.id} missing effort_levels`).toBeTruthy()
  }

  const claude = data.providers.find((p) => p.id === 'claude')
  expect(claude, 'claude provider present').toBeTruthy()
  const claudeIds = (claude!.effort_levels ?? []).map((e) => e.id)
  // The added levels are the crux of this change.
  expect(claudeIds).toContain('xhigh')
  expect(claudeIds).toContain('max')
  expect(claudeIds).toEqual(['low', 'medium', 'high', 'xhigh', 'max'])
})

test('effort dropdown loads provider levels and a max-effort choice round-trips', async ({
  page,
  baseURL,
  request,
}) => {
  expect(baseURL).toBeTruthy()
  const { authHeader } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-effort-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-effort-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  await loginUi(page, baseURL!)
  await expect(page.locator('.rail')).toBeVisible()
  await page.locator('.rail-btn[title="Repeating Tasks"]').click()
  await expect(page.getByRole('heading', { name: 'Repeating Tasks' })).toBeVisible()

  await page.getByRole('button', { name: /new task/i }).click()
  await expect(page.getByRole('heading', { name: /new repeating task/i })).toBeVisible()

  // Name deliberately avoids the substring "effort" — a leftover tab's
  // close-button aria-label would otherwise collide with other specs'
  // non-exact getByLabel('Effort') lookups on the shared test server.
  const taskName = `ladder-task-${Date.now()}`
  await page.getByPlaceholder(/daily project sweep/i).fill(taskName)
  await page.locator('select.form-input').first().selectOption({ value: folder.id })
  await page.getByPlaceholder(/message sent to the new session/i).fill('do thing')

  // Before a model is picked the default provider (claude) drives the options,
  // so the added Extra high / Max levels are already offered.
  const effort = page.getByLabel('Effort', { exact: true })
  await expect(effort.locator('option[value="xhigh"]')).toHaveCount(1)
  await expect(effort.locator('option[value="max"]')).toHaveCount(1)

  // Pick the deterministic mock model; its provider also exposes the full
  // ladder, so Extra high / Max remain available to choose.
  await page.getByTestId('repeating-task-model').click()
  await page.getByTestId('repeating-task-model-search').fill('happy')
  await page.getByRole('option', { name: 'Mock: happy path' }).click()
  await expect(effort.locator('option[value="max"]')).toHaveCount(1)

  await effort.selectOption({ value: 'max' })
  await page.getByRole('button', { name: /create task/i }).click()

  // Detail view reflects the provider-effort choice…
  await expect(page.getByRole('heading', { name: taskName })).toBeVisible()
  await expect(page.getByText('mock:happy-path')).toBeVisible()
  await expect(page.getByText('max', { exact: true })).toBeVisible()

  // …and it actually persisted on the task.
  const listRes = await request.get('/api/repeating-tasks', { headers: authHeader })
  const tasks = (await listRes.json()) as Array<{
    name: string
    model: string | null
    effort: string | null
  }>
  const created = tasks.find((t) => t.name === taskName)
  expect(created, 'created task missing from list').toBeTruthy()
  expect(created!.model).toBe('mock:happy-path')
  expect(created!.effort).toBe('max')
})
