import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * A stored app-wide default model whose provider is gone (ollama rm, a
 * plugin uninstall) must not silently pin itself: forms that preselect the
 * default treat a dead id as unset, warn inline, and submit no model at
 * all — dispatch falls back to the backend's routing.
 *
 * Covered surface: the New Session modal. The same shared helper
 * (`modelGoneFromCatalogue`) and notice component back CardFormModal,
 * NewRepeatingTaskModal and ReviewView.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
/** Set as the app default but never listed by any provider's catalogue. */
const DEAD_MODEL = 'mock:model-that-was-removed'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

test('a dead default model warns in New Session and creates without a pin', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-deadmodel-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-deadmodel-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()

  // The dead id must actually be dead: absent from the live catalogue.
  const modelsRes = await request.get('/api/models', { headers: authHeader })
  expect(modelsRes.ok()).toBeTruthy()
  const { models } = (await modelsRes.json()) as { models: { id: string }[] }
  expect(models.some((m) => m.id === DEAD_MODEL)).toBe(false)

  const put = await request.put('/api/settings/default-model', {
    headers: authHeader,
    data: { model: DEAD_MODEL },
  })
  expect(put.status(), 'set default model').toBe(204)

  try {
    await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
    await page.goto('/')
    await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })

    await page.locator('.tab-new').click()
    await expect(page.getByTestId('new-session-preset')).toBeVisible()

    // The dead default must not preselect; the form warns and shows an
    // empty picker instead of the raw dead id.
    await expect(page.getByTestId('model-gone-notice')).toContainText('no longer available', {
      timeout: 10_000,
    })
    await expect(page.getByTestId('model-gone-notice')).toContainText(DEAD_MODEL)
    await expect(page.getByTestId('new-session-model')).toContainText('Select model…')

    const name = `dead-default-${Date.now()}`
    await page.locator('#new-session-name').fill(name)
    await page.getByRole('button', { name: 'Create Session' }).click()

    await expect(page.locator('.chat-container')).toBeVisible({ timeout: 10_000 })
    await expect(page).toHaveURL(/\/sessions\/[0-9a-f-]+$/)
    const sessionId = page.url().split('/').pop()!

    const sessionRes = await request.get(`/api/sessions/${sessionId}`, { headers: authHeader })
    expect(sessionRes.ok(), `get session failed: ${await sessionRes.text()}`).toBeTruthy()
    const session = (await sessionRes.json()) as { model?: string | null }
    // The untouched form must NOT have submitted the dead id as a pin.
    expect(session.model ?? null).toBeNull()
  } finally {
    // Global setting — clear it so later specs see the pristine unset state.
    await request.put('/api/settings/default-model', {
      headers: authHeader,
      data: { model: '' },
    })
  }
})
