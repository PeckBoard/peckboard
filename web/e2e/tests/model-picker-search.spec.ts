import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * The model picker (ModelPicker.tsx) is a searchable combobox: clicking the
 * trigger opens a popup with a filter input over the model catalogue. This
 * proves the type-to-filter behaviour in the New Session modal — the same
 * component backs the session toolbar, project, card, and automation pickers.
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

async function seedSession(
  request: APIRequestContext,
  authHeader: Record<string, string>,
): Promise<{ sessionId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-mp-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-mp-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'seed session', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  return { sessionId: session.id }
}

test('model picker filters the catalogue as you type', async ({ request, page, baseURL }) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedSession(request, authHeader)

  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto(`/sessions/${sessionId}`)
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })

  // Open the New Session modal via the tab strip's "+" button.
  await page.locator('.tab-new').click()

  // The model field is now a combobox trigger, not a native <select>.
  const trigger = page.getByTestId('new-session-model')
  await expect(trigger).toBeVisible({ timeout: 10_000 })
  // Nothing chosen yet → shows the default label.
  await expect(trigger).toContainText('Server default')

  await trigger.click()
  const search = page.getByTestId('new-session-model-search')
  await expect(search).toBeVisible()

  // Several mock models are listed before filtering.
  await expect(page.getByRole('option', { name: 'Mock: happy path' })).toBeVisible()
  await expect(page.getByRole('option', { name: 'Mock: echo' })).toBeVisible()

  // Typing narrows the list to matches only.
  await search.fill('happy')
  await expect(page.getByRole('option', { name: 'Mock: happy path' })).toBeVisible()
  await expect(page.getByRole('option', { name: 'Mock: echo' })).toHaveCount(0)
  await expect(page.getByRole('option', { name: 'Mock: crash' })).toHaveCount(0)

  // Selecting closes the popup and updates the trigger label.
  await page.getByRole('option', { name: 'Mock: happy path' }).click()
  await expect(search).toHaveCount(0)
  await expect(trigger).toContainText('Mock: happy path')

  // And the choice actually drives session creation.
  await page.getByPlaceholder('My session').fill('picker-test')
  await page.getByRole('button', { name: 'Create Session' }).click()

  await expect
    .poll(async () => {
      const res = await request.get('/api/sessions', { headers: authHeader })
      const { items } = (await res.json()) as {
        items: Array<{ name: string; model: string | null }>
      }
      return items.find((s) => s.name === 'picker-test')?.model ?? null
    })
    .toBe('mock:happy-path')
})
