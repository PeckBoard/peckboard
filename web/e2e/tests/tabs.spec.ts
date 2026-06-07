import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E for the cross-device tab strip.
 *
 * Flows covered (one spec, multiple test blocks — each is a distinct
 * user-visible behaviour):
 *   1. Opening a session adds a tab and the tab becomes active.
 *   2. Opening a second session promotes it to the front (MRU) and the
 *      first tab is still there but no longer active.
 *   3. Reloading the page restores the tabs from the server.
 *   4. Right-click on a tab opens a context menu, "Close tab" removes
 *      it from the strip (server-side too — survives reload).
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

let cachedAuth: { token: string; auth: Record<string, string> } | null = null

/** Authenticate once per spec file. The rate limiter sees all requests
 *  from 127.0.0.1 as one client; re-authenticating in every test would
 *  burn the budget within a single suite run. */
async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  if (cachedAuth) return cachedAuth
  const status = await request.get('/api/auth/status')
  const { has_users } = (await status.json()) as { has_users: boolean }
  const endpoint = has_users ? '/api/auth/login' : '/api/auth/register'
  const res = await request.post(endpoint, {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok()).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  cachedAuth = { token, auth: { Authorization: `Bearer ${token}` } }
  return cachedAuth
}

async function seedFolderAndSession(
  request: APIRequestContext,
  auth: Record<string, string>,
  sessionName: string,
): Promise<{ sessionId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-tabs-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'e2e-tabs', path: folderPath },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: sessionName, folder_id: folder.id },
  })
  expect(sessionRes.ok()).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  return { sessionId: session.id }
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })
}

/** Clear server-side tab list so a test starts from a clean slate. */
async function clearTabs(request: APIRequestContext, auth: Record<string, string>) {
  const res = await request.get('/api/me/tabs', { headers: auth })
  if (!res.ok()) return
  const tabs = (await res.json()) as Array<{ item_type: string; item_id: string }>
  for (const t of tabs) {
    await request.delete(`/api/me/tabs/${t.item_type}/${t.item_id}`, { headers: auth })
  }
}

test.describe('tabs', () => {
  test('opening a session adds a tab and marks it active', async ({ request, page, baseURL }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)
    const { sessionId } = await seedFolderAndSession(request, auth, 'alpha')

    await loadAt(page, token, `/sessions/${sessionId}`)

    // The tab should appear, labelled with the session name, and be
    // the active one (the chip with the accent underline).
    const tab = page.locator('.tab-opened', { hasText: 'alpha' })
    await expect(tab).toBeVisible({ timeout: 5_000 })
    await expect(tab).toHaveClass(/tab-active/)
  })

  test('opening a second session bumps it to MRU front; first stays', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)
    const { sessionId: idA } = await seedFolderAndSession(request, auth, 'alpha')
    const { sessionId: idB } = await seedFolderAndSession(request, auth, 'beta')

    await loadAt(page, token, `/sessions/${idA}`)
    await expect(page.locator('.tab-opened', { hasText: 'alpha' })).toHaveClass(/tab-active/)

    // Navigate to the second session via the SPA (router push).
    await page.evaluate((id) => {
      history.pushState(null, '', `/sessions/${id}`)
      window.dispatchEvent(new PopStateEvent('popstate'))
    }, idB)

    // Both tabs present; beta is active and first (MRU).
    const openedTabs = page.locator('.tab-opened')
    await expect(openedTabs).toHaveCount(2)
    await expect(openedTabs.first()).toContainText('beta')
    await expect(openedTabs.first()).toHaveClass(/tab-active/)
    await expect(openedTabs.nth(1)).toContainText('alpha')
    await expect(openedTabs.nth(1)).not.toHaveClass(/tab-active/)
  })

  test('tabs persist across a full page reload (server-backed)', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)
    const { sessionId } = await seedFolderAndSession(request, auth, 'persistent-session')

    await loadAt(page, token, `/sessions/${sessionId}`)
    await expect(page.locator('.tab-opened', { hasText: 'persistent-session' })).toBeVisible()

    await page.reload()
    await expect(page.locator('.tab-opened', { hasText: 'persistent-session' })).toBeVisible({
      timeout: 5_000,
    })
  })

  test('right-click → Close removes the tab (server-side too)', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)
    const { sessionId } = await seedFolderAndSession(request, auth, 'closeable')

    await loadAt(page, token, `/sessions/${sessionId}`)
    const tab = page.locator('.tab-opened', { hasText: 'closeable' })
    await expect(tab).toBeVisible()

    await tab.click({ button: 'right' })
    const closeBtn = page.locator('.tab-menu button', { hasText: 'Close tab' })
    await expect(closeBtn).toBeVisible()
    await closeBtn.click()

    await expect(tab).toHaveCount(0)

    // Confirm server agrees: list_tabs returns no entry for this session.
    // (Tiny wait so the optimistic-then-write DELETE has flushed.)
    await page.waitForTimeout(200)
    const res = await request.get('/api/me/tabs', { headers: auth })
    expect(res.ok(), `GET tabs failed (${res.status()}): ${await res.text()}`).toBeTruthy()
    const body = await res.json()
    const tabList = Array.isArray(body) ? body : []
    expect(
      tabList.find((t: { item_id: string }) => t.item_id === sessionId),
      `tab should be gone server-side; got: ${JSON.stringify(body)}`,
    ).toBeUndefined()
  })

  test('switching to an already-open tab does not reorder the strip', async ({
    request,
    page,
    baseURL,
  }) => {
    // Regression test: clicking an existing tab used to bump it to the
    // front via `last_active` MRU, which made the strip shuffle on every
    // navigation. Now the order should only change when a brand-new tab
    // is opened.
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)
    const { sessionId: idA } = await seedFolderAndSession(request, auth, 'first-open')
    const { sessionId: idB } = await seedFolderAndSession(request, auth, 'second-open')

    await loadAt(page, token, `/sessions/${idA}`)
    await page.evaluate((id) => {
      history.pushState(null, '', `/sessions/${id}`)
      window.dispatchEvent(new PopStateEvent('popstate'))
    }, idB)

    // Strip should be [second-open, first-open] (newest insertion first).
    let openedTabs = page.locator('.tab-opened')
    await expect(openedTabs).toHaveCount(2)
    await expect(openedTabs.first()).toContainText('second-open')
    await expect(openedTabs.nth(1)).toContainText('first-open')

    // Switch back to A by clicking its tab in the strip. A must become
    // active but B must stay at the front — clicking is selection, not
    // reordering.
    await openedTabs.nth(1).click()
    openedTabs = page.locator('.tab-opened')
    await expect(openedTabs).toHaveCount(2)
    await expect(openedTabs.first()).toContainText('second-open')
    await expect(openedTabs.first()).not.toHaveClass(/tab-active/)
    await expect(openedTabs.nth(1)).toContainText('first-open')
    await expect(openedTabs.nth(1)).toHaveClass(/tab-active/)
  })

  test('right-click → Delete session removes both the session and its tab', async ({
    request,
    page,
    baseURL,
  }) => {
    // Regression test: deleting a session used to leave the tab dangling.
    // The tab fell back to its placeholder label ("Session"), which the
    // user saw as "the session got replaced with a new one named
    // Session". Tab and session must disappear together.
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)
    const { sessionId } = await seedFolderAndSession(request, auth, 'doomed')

    await loadAt(page, token, `/sessions/${sessionId}`)
    const tab = page.locator('.tab-opened', { hasText: 'doomed' })
    await expect(tab).toBeVisible()

    await tab.click({ button: 'right' })
    const deleteBtn = page.locator('.tab-menu button', { hasText: 'Delete session' })
    await expect(deleteBtn).toBeVisible()
    await deleteBtn.click()

    // ConfirmDialog appears. The danger button is labelled "Delete".
    const confirmBtn = page.locator('.confirm-dialog-danger', { hasText: /^Delete$/ })
    await expect(confirmBtn).toBeVisible()
    await confirmBtn.click()

    // No tab with this label, and crucially no orphan tab labelled
    // "Session" (the placeholder for a tab whose session has been
    // deleted out from under it).
    await expect(page.locator('.tab-opened', { hasText: 'doomed' })).toHaveCount(0)
    await expect(page.locator('.tab-opened', { hasText: /^Session$/ })).toHaveCount(0)

    // Server-side too: both the session row and the user_tabs row must
    // be gone, so a refetch (or a new device login) won't resurrect it.
    await page.waitForTimeout(200)
    const tabsRes = await request.get('/api/me/tabs', { headers: auth })
    expect(tabsRes.ok()).toBeTruthy()
    const tabsBody = (await tabsRes.json()) as Array<{ item_id: string }>
    expect(tabsBody.find((t) => t.item_id === sessionId)).toBeUndefined()

    const sessionRes = await request.get(`/api/sessions/${sessionId}`, { headers: auth })
    expect(sessionRes.status()).toBe(404)
  })
})
