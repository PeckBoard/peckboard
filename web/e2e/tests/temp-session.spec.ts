import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E for temp sessions: a session created with the "Temporary" checkbox
 * is deleted outright — not just untabbed — when its tab chip is closed.
 * The chip carries an hourglass marker so the destructive close is
 * signposted, and closing a REGULAR session's tab must keep the session.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function login(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { token: string }).token
}

async function createFolder(
  request: APIRequestContext,
  auth: Record<string, string>,
): Promise<string> {
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'temp-folder', path: mkdtempSync(path.join(tmpdir(), 'pb-temp-')) },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function loadAs(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
  await expect(page.locator('.rail-avatar')).toBeVisible({ timeout: 10_000 })
}

test('temp session: hourglass chip, and closing the tab deletes the session', async ({
  request,
  page,
}) => {
  const token = await login(request)
  const auth = { Authorization: `Bearer ${token}` }
  await createFolder(request, auth)

  await loadAs(page, token, '/')

  // Create through the New Session modal with the Temporary checkbox on.
  const name = `temp-e2e-${Date.now()}`
  await page.locator('.tab-new').click()
  await page.getByPlaceholder('My session').fill(name)
  await page.getByTestId('new-session-temp').check()
  await page.screenshot({ path: 'test-results/temp-session-modal.png' })
  await page.getByRole('button', { name: 'Create Session' }).click()

  // The session opens; its tab chip carries the temp (hourglass) marker.
  const chip = page.locator('.tab-wrap', { hasText: name })
  await expect(chip).toBeVisible()
  await expect(chip.locator('.tab-icon-temp-session')).toBeVisible()
  await page.screenshot({ path: 'test-results/temp-session-chip.png' })

  // Grab the id via the API so we can assert server-side deletion.
  const tabs = (await (await request.get('/api/me/tabs', { headers: auth })).json()) as {
    item_id: string
    name: string
    is_temp: boolean
  }[]
  const ours = tabs.find((t) => t.name === name)
  expect(ours, 'temp session tab should be listed').toBeTruthy()
  expect(ours?.is_temp).toBe(true)
  const id = ours!.item_id

  // Close the chip — for a temp session this deletes the session itself.
  await chip.locator('.tab-close').click()
  await expect(chip).toHaveCount(0)

  // Server-side: the session row is gone.
  await expect
    .poll(async () => (await request.get(`/api/sessions/${id}`, { headers: auth })).status(), {
      message: 'temp session should 404 after its tab closes',
    })
    .toBe(404)

  // And the sessions list view no longer shows it.
  await expect(page.locator('.list-view-row', { hasText: name })).toHaveCount(0)
  await page.screenshot({ path: 'test-results/temp-session-after-close.png' })
})

test('regular session survives closing its tab', async ({ request, page }) => {
  const token = await login(request)
  const auth = { Authorization: `Bearer ${token}` }
  const folderId = await createFolder(request, auth)

  const name = `keep-e2e-${Date.now()}`
  const res = await request.post('/api/sessions', {
    headers: auth,
    data: { name, folder_id: folderId },
  })
  expect(res.ok()).toBeTruthy()
  const session = (await res.json()) as { id: string; is_temp: boolean }
  expect(session.is_temp).toBe(false)

  await loadAs(page, token, '/')

  // Open it from the list so a tab chip exists, then close the chip.
  await page.locator('.list-view-row', { hasText: name }).locator('.list-view-item').click()
  const chip = page.locator('.tab-wrap', { hasText: name })
  await expect(chip).toBeVisible()
  await expect(chip.locator('.tab-icon-temp-session')).toHaveCount(0)
  await chip.locator('.tab-close').click()
  await expect(chip).toHaveCount(0)

  // The session itself is untouched.
  const after = await request.get(`/api/sessions/${session.id}`, { headers: auth })
  expect(after.status()).toBe(200)
})
