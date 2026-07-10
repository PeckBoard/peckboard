import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E for the fix: explicit re-activation must re-store a tab whose chip was
 * removed by a background close (another device's server-side DELETE).
 *
 * The same-id gap: when activeSessionId is already S and the server-side tab
 * row is deleted, App.tsx's edge-guard effect is gated on the active-id
 * CHANGING — so a same-id user action (e.g. "View Session" while already on
 * S) never fires the effect and the chip stays gone.  The fix calls
 * openTab() directly at every user-activation call site, bypassing the edge.
 *
 * Test flow:
 *  (a) Open session S → chip visible and active.
 *  (b) DELETE the tab row via API (simulates another device closing).
 *  (c) Trigger in-page refetch (window focus) → assert chip disappears.
 *  (d) Navigate to the sessions list via SPA (exercises our fixed onActivate).
 *  (e) Click session S in the list → chip must re-appear.
 *  (f) GET /api/me/tabs → row must be present server-side.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

let cachedAuth: { token: string; auth: Record<string, string> } | null = null

async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  if (cachedAuth) return cachedAuth
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok()).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  cachedAuth = { token, auth: { Authorization: `Bearer ${token}` } }
  return cachedAuth
}

async function clearTabs(request: APIRequestContext, auth: Record<string, string>) {
  const res = await request.get('/api/me/tabs', { headers: auth })
  if (!res.ok()) return
  const tabs = (await res.json()) as Array<{ item_type: string; item_id: string }>
  for (const t of tabs) {
    await request.delete(`/api/me/tabs/${t.item_type}/${encodeURIComponent(t.item_id)}`, {
      headers: auth,
    })
  }
}

async function seedSession(
  request: APIRequestContext,
  auth: Record<string, string>,
  name: string,
): Promise<string> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-reactivate-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'e2e-reactivate', path: folderPath },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name, folder_id: folder.id },
  })
  expect(sessionRes.ok()).toBeTruthy()
  return ((await sessionRes.json()) as { id: string }).id
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })
}

test.describe('tabs-reactivate-same-id', () => {
  test('background-closed session chip re-appears when user explicitly re-activates it', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)

    const sessionId = await seedSession(request, auth, 'reactivate-session')

    // (a) Open the session — chip must become visible and active.
    await loadAt(page, token, `/sessions/${sessionId}`)
    const chip = page.locator('.tab-opened', { hasText: 'reactivate-session' })
    await expect(chip).toBeVisible({ timeout: 5_000 })
    await expect(chip).toHaveClass(/tab-active/)

    // (b) DELETE the tab row server-side (simulate another device closing).
    const deleteRes = await request.delete(`/api/me/tabs/session/${sessionId}`, {
      headers: auth,
    })
    expect(deleteRes.ok()).toBeTruthy()

    // (c) Trigger in-page refetch via window focus, then assert chip gone.
    await page.evaluate(() => window.dispatchEvent(new Event('focus')))
    await expect(chip).toHaveCount(0, { timeout: 5_000 })

    // Server-side delete confirmed: chip should be gone locally.
    const tabsBefore = await request.get('/api/me/tabs', { headers: auth })
    expect(tabsBefore.ok()).toBeTruthy()
    const listBefore = (await tabsBefore.json()) as Array<{ item_type: string; item_id: string }>
    expect(
      listBefore.find((t) => t.item_type === 'session' && t.item_id === sessionId),
      `session tab must be absent server-side before re-activation; got: ${JSON.stringify(listBefore)}`,
    ).toBeUndefined()

    // (d) Navigate to the sessions list via SPA (activates our fixed onActivate
    // path by making the list visible; activeSessionId → null here).
    await page.evaluate(() => {
      history.pushState(null, '', '/sessions')
      window.dispatchEvent(new PopStateEvent('popstate'))
    })

    // Wait for the sessions list to be rendered (chip gone, list visible).
    await expect(page.locator('.list-view-body')).toBeVisible({ timeout: 5_000 })

    // (e) Click session in list — our fixed onActivate calls openTab('session', id)
    // directly, bypassing the edge-guard, so the chip must be re-created even
    // when activeSessionId was already set to the same id before this step.
    const listItem = page.locator('.list-view-item', { hasText: 'reactivate-session' }).first()
    await listItem.click()

    const restoredChip = page.locator('.tab-opened', { hasText: 'reactivate-session' })
    await expect(restoredChip).toBeVisible({ timeout: 5_000 })
    await expect(restoredChip).toHaveClass(/tab-active/)

    // (f) Assert the row is present server-side.
    await page.waitForTimeout(300)
    const tabsAfter = await request.get('/api/me/tabs', { headers: auth })
    expect(tabsAfter.ok()).toBeTruthy()
    const listAfter = (await tabsAfter.json()) as Array<{ item_type: string; item_id: string }>
    expect(
      listAfter.find((t) => t.item_type === 'session' && t.item_id === sessionId),
      `session tab must be present server-side after re-activation; got: ${JSON.stringify(listAfter)}`,
    ).toBeDefined()
  })
})
