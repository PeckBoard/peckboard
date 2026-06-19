import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Cross-device session-delete behaviour.
 *
 * The original bug: device A is sitting on `/sessions/<X>` with the tab
 * strip showing X and the body rendering ChatView. Device B deletes X.
 * Device A used to keep the body mounted against the now-404'd session
 * — the only cleanup path was the focus-driven `/api/me/tabs` refetch,
 * which dropped the tab but never touched the activeSessionId, so the
 * chat view stayed open as a ghost UI on top of a missing session.
 *
 * Fix: backend broadcasts a `session-deleted` WS frame to every
 * connected client; the frontend ws store routes that through
 * `applySessionDeleted`, which drops the row, wipes cached events,
 * closes the tab, and clears activeSessionId if it matched. This spec
 * pins both halves: another-client delete is observed by this client,
 * and a same-client delete still ends in the same state.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok()).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function seedSession(request: APIRequestContext, auth: Record<string, string>, name: string) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-sd-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'sd', path: folderPath },
  })
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name, folder_id: folder.id },
  })
  return ((await sessionRes.json()) as { id: string }).id
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

test('another device deleting the active session clears tab AND body', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL).toBeTruthy()
  const { token, auth } = await authenticate(request)
  const sessionId = await seedSession(request, auth, 'cross-device-target')

  await loadAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-container')).toBeVisible({ timeout: 5_000 })
  await expect(page.locator('.tab-opened', { hasText: 'cross-device-target' })).toBeVisible()

  // Simulate another device's DELETE via the same backing API — bypass
  // the in-page store so this is genuinely the "remote delete" path
  // and not just a local optimistic update.
  const res = await request.delete(`/api/sessions/${sessionId}`, { headers: auth })
  expect(res.ok(), `delete failed: ${res.status()}`).toBeTruthy()

  // The session-deleted broadcast must reach this client and unmount
  // the chat body — leaving it mounted is the regression we're guarding
  // against. List view replaces it.
  await expect(page.locator('.chat-container')).toHaveCount(0, { timeout: 5_000 })
  await expect(page.locator('.list-view')).toBeVisible({ timeout: 5_000 })
  // The tab strip entry is gone too — no orphan "Session"-labelled chip
  // hanging around after the underlying session vanished.
  await expect(page.locator('.tab-opened', { hasText: 'cross-device-target' })).toHaveCount(0)
  await expect(page.locator('.tab-opened', { hasText: /^Session$/ })).toHaveCount(0)
  // URL drops back to the session list since activeSessionId was cleared.
  await expect.poll(() => new URL(page.url()).pathname).toBe('/')
})

test('deleting an active session via the in-page tab still works (no regression)', async ({
  request,
  page,
  baseURL,
}) => {
  // The local-delete path was always supposed to switch the body to the
  // list view, but nothing pinned the body state — only the tab state.
  // Lock both in now that the same code path runs for local and remote
  // deletes (deleteSession → applySessionDeleted).
  expect(baseURL).toBeTruthy()
  const { token, auth } = await authenticate(request)
  const sessionId = await seedSession(request, auth, 'local-target')

  await loadAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-container')).toBeVisible({ timeout: 5_000 })

  const tab = page.locator('.tab-opened', { hasText: 'local-target' })
  await tab.click({ button: 'right' })
  await page.locator('.context-menu button', { hasText: 'Delete session' }).click()
  await page.locator('.confirm-dialog-danger', { hasText: /^Delete$/ }).click()

  await expect(page.locator('.chat-container')).toHaveCount(0, { timeout: 5_000 })
  await expect(page.locator('.list-view')).toBeVisible({ timeout: 5_000 })
  await expect(page.locator('.tab-opened', { hasText: 'local-target' })).toHaveCount(0)
})
