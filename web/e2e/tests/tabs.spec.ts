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

async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  const status = await request.get('/api/auth/status')
  const { has_users } = (await status.json()) as { has_users: boolean }
  const endpoint = has_users ? '/api/auth/login' : '/api/auth/register'
  const res = await request.post(endpoint, {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok()).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
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
    const closeBtn = page.locator('.tab-menu', { hasText: 'Close tab' })
    await expect(closeBtn).toBeVisible()
    await closeBtn.click()

    await expect(tab).toHaveCount(0)

    // Confirm server agrees: list_tabs returns no entry for this session.
    const res = await request.get('/api/me/tabs', { headers: auth })
    const tabs = (await res.json()) as Array<{ item_id: string }>
    expect(tabs.find((t) => t.item_id === sessionId)).toBeUndefined()
  })
})
