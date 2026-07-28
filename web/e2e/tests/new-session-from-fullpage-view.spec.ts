import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Creating a session must land the user IN that session, from anywhere in
 * the app.
 *
 * `App.tsx` holds one `view` selector for the full-page views (Settings,
 * Usage, Reports, Folders, Users, …) and the activeSessionId→URL effect only
 * runs while `view === 'sessions'`. The New Session modal used to only call
 * `setActiveSession`, so creating from Settings left the overlay up with the
 * new tab chip present but unselected. The modal now reports the new id via
 * `onCreated` and the host does the full activation (select + navigate +
 * open tab).
 *
 * Covered here: Settings and Usage (the same overlay mechanism, so one
 * other full-page view is enough to prove it is not Settings-specific).
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
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-fullpage-'))
  const res = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-fullpage-${Date.now()}`, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto(route)
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })
}

/** Drive the New Session modal end to end against the deterministic mock
 *  provider, from whatever view is currently open. */
async function createSessionViaModal(page: Page, name: string) {
  await page.locator('.tab-new').click()
  await expect(page.getByTestId('new-session-preset')).toBeVisible()
  await page.locator('#new-session-name').fill(name)
  await page.getByTestId('new-session-model').click()
  await page.getByTestId('new-session-model-search').fill('happy')
  await page.getByRole('option', { name: 'Mock: happy path' }).click()
  await page.getByRole('button', { name: 'Create Session' }).click()
}

/** The new session is the visible view and its chip is the selected one. */
async function expectLandedInSession(page: Page, name: string) {
  await expect(page.locator('.chat-container')).toBeVisible({ timeout: 10_000 })
  await expect(page.locator('.chat-toolbar-name')).toHaveText(name)
  await expect(page).toHaveURL(/\/sessions\/[0-9a-f-]+$/)
  const chip = page.locator('.tab-opened', { hasText: name })
  await expect(chip).toBeVisible()
  await expect(chip).toHaveClass(/tab-active/)
}

test('creating a session from Settings opens it and selects its tab', async ({ request, page }) => {
  const { token, authHeader } = await authenticate(request)
  await createFolder(request, authHeader)

  await loadAt(page, token, '/settings')
  await expect(page.getByTestId('settings-page')).toBeVisible({ timeout: 10_000 })

  const name = `from-settings-${Date.now()}`
  await createSessionViaModal(page, name)

  // The Settings overlay must be dismissed, not merely covered.
  await expect(page.getByTestId('settings-page')).toHaveCount(0)
  await expectLandedInSession(page, name)
})

test('creating a session from Usage opens it and selects its tab', async ({ request, page }) => {
  const { token, authHeader } = await authenticate(request)
  await createFolder(request, authHeader)

  await loadAt(page, token, '/usage')
  await expect(page.getByTestId('usage-view')).toBeVisible({ timeout: 10_000 })

  const name = `from-usage-${Date.now()}`
  await createSessionViaModal(page, name)

  await expect(page.getByTestId('usage-view')).toHaveCount(0)
  await expectLandedInSession(page, name)
})
