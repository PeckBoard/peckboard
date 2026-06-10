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
 *   5. Right-click → Rename updates the tab label and persists.
 *   6. Right-click → Clear messages confirms and POSTs the clear endpoint.
 *   7. Project tabs hide the session-only "Clear messages" item and
 *      label the destructive action "Delete project".
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
  // The server auto-bootstraps the admin from PECKBOARD_BOOTSTRAP_*
  // env vars at first start (see playwright.config.ts); we just log in.
  const res = await request.post('/api/auth/login', {
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
    const closeBtn = page.locator('.context-menu button', { hasText: 'Close tab' })
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
    // Wait for A's tab to actually land in the strip before navigating
    // to B — without this, B can be opened first and the MRU prepend
    // order ends up [A, B] instead of [B, A]. (Previously the loadAt
    // helper indirectly waited because `.tabbar` only rendered once
    // there was at least one tab. Now the strip is always visible for
    // the trailing `+` button, so we wait explicitly.)
    // Wait until A is the *sole, active* tab — not merely present. The tab
    // strip reconciles against the server-persisted order after each
    // navigation, so opening B before A has fully committed can leave the
    // strip settling as [A, B] instead of [B, A]. Gating on count===1 +
    // active closes that window.
    const firstTab = page.locator('.tab-opened', { hasText: 'first-open' })
    await expect(firstTab).toBeVisible({ timeout: 5_000 })
    await expect(page.locator('.tab-opened')).toHaveCount(1)
    await expect(firstTab).toHaveClass(/tab-active/)
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
    const deleteBtn = page.locator('.context-menu button', { hasText: 'Delete session' })
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

  test('navigating to a deleted session id does not create a phantom tab', async ({
    request,
    page,
    baseURL,
  }) => {
    // Regression test: a stale URL like a bookmark or browser history
    // entry for `/sessions/<deleted-id>` used to trigger an auto-openTab
    // for that id, writing a phantom `user_tabs` row that rendered as
    // an orphan chip labelled "Session". Visiting an unknown id should
    // just drop back to the list view without leaving a tab behind.
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)

    // Use a syntactically valid but non-existent session id. Going
    // through page.goto won't render `.tabbar` (no tabs to show), so
    // assert on the URL + tab list directly.
    await page.addInitScript((t) => {
      localStorage.setItem('peckboard_token', t)
    }, token)
    await page.goto('/sessions/00000000-0000-0000-0000-000000000000')

    // The shell renders; the rail is the cheapest stable thing to wait
    // on without depending on which view is showing.
    await expect(page.locator('.rail')).toBeVisible({ timeout: 10_000 })

    // No tab strip should render — there shouldn't be any tabs at all.
    await expect(page.locator('.tab-opened')).toHaveCount(0)
    await expect(page.locator('.tab-opened', { hasText: /^Session$/ })).toHaveCount(0)

    // URL should have been cleared back to the session list (`/`).
    await expect.poll(() => new URL(page.url()).pathname).toBe('/')

    // And server-side, no phantom tab should have been created.
    await page.waitForTimeout(200)
    const tabsRes = await request.get('/api/me/tabs', { headers: auth })
    expect(tabsRes.ok()).toBeTruthy()
    expect((await tabsRes.json()) as unknown[]).toEqual([])
  })

  test('right-click → Rename updates the tab label and persists', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)
    const { sessionId } = await seedFolderAndSession(request, auth, 'old-name')

    await loadAt(page, token, `/sessions/${sessionId}`)
    const tab = page.locator('.tab-opened', { hasText: 'old-name' })
    await expect(tab).toBeVisible()

    // Answer the window.prompt() with the new name before triggering it.
    page.once('dialog', (dialog) => {
      void dialog.accept('new-name')
    })

    await tab.click({ button: 'right' })
    const renameBtn = page.locator('.context-menu button', { hasText: 'Rename' })
    await expect(renameBtn).toBeVisible()
    await renameBtn.click()

    // Tab label flips to the new name; server agrees.
    await expect(page.locator('.tab-opened', { hasText: 'new-name' })).toBeVisible({
      timeout: 5_000,
    })
    await expect(page.locator('.tab-opened', { hasText: 'old-name' })).toHaveCount(0)

    const sessionRes = await request.get(`/api/sessions/${sessionId}`, { headers: auth })
    expect(sessionRes.ok()).toBeTruthy()
    const sessionBody = (await sessionRes.json()) as { name: string }
    expect(sessionBody.name).toBe('new-name')
  })

  test('right-click → Clear messages confirms then POSTs the clear endpoint', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)
    const { sessionId } = await seedFolderAndSession(request, auth, 'clearable')

    await loadAt(page, token, `/sessions/${sessionId}`)
    const tab = page.locator('.tab-opened', { hasText: 'clearable' })
    await expect(tab).toBeVisible()

    await tab.click({ button: 'right' })
    const clearBtn = page.locator('.context-menu button', { hasText: 'Clear messages' })
    await expect(clearBtn).toBeVisible()
    await clearBtn.click()

    // ConfirmDialog appears with a Clear danger button (not Delete).
    const confirmBtn = page.locator('.confirm-dialog-danger', { hasText: /^Clear$/ })
    await expect(confirmBtn).toBeVisible()

    // Watch for the POST to the clear endpoint so we verify wiring even
    // if the session has no events to assert against.
    const clearResponse = page.waitForResponse(
      (res) =>
        res.url().endsWith(`/api/sessions/${sessionId}/clear`) && res.request().method() === 'POST',
    )
    await confirmBtn.click()
    const cleared = await clearResponse
    expect(cleared.ok()).toBeTruthy()

    // Dialog dismisses and server-side events list is empty.
    await expect(page.locator('.confirm-dialog-title')).toHaveCount(0)
    const eventsRes = await request.get(`/api/sessions/${sessionId}/events`, { headers: auth })
    expect(eventsRes.ok()).toBeTruthy()
    expect((await eventsRes.json()) as unknown[]).toEqual([])
  })

  test('project tab context menu omits Clear messages and labels Delete as project', async ({
    request,
    page,
    baseURL,
  }) => {
    // Clear messages is session-specific. The same menu rendered on a
    // project tab should hide that item entirely and offer Delete
    // project (not Delete session).
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)

    // Seed a folder + project (no cards needed — we only exercise the menu).
    const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-tabs-proj-'))
    const folderRes = await request.post('/api/folders', {
      headers: auth,
      data: { name: 'e2e-tabs-proj', path: folderPath },
    })
    expect(folderRes.ok()).toBeTruthy()
    const folder = (await folderRes.json()) as { id: string }
    const projectRes = await request.post('/api/projects', {
      headers: auth,
      data: {
        name: 'menu-proj',
        folder_id: folder.id,
        worker_count: 1,
        model: 'mock:happy-path',
        workflow: 'task',
      },
    })
    expect(projectRes.ok()).toBeTruthy()
    const project = (await projectRes.json()) as { id: string }

    await loadAt(page, token, `/projects/${project.id}`)
    const tab = page.locator('.tab-opened', { hasText: 'menu-proj' })
    await expect(tab).toBeVisible()

    await tab.click({ button: 'right' })
    await expect(page.locator('.context-menu')).toBeVisible()

    await expect(page.locator('.context-menu button', { hasText: 'Close tab' })).toBeVisible()
    await expect(page.locator('.context-menu button', { hasText: 'Rename' })).toBeVisible()
    await expect(page.locator('.context-menu button', { hasText: 'Delete project' })).toBeVisible()
    await expect(page.locator('.context-menu button', { hasText: 'Clear messages' })).toHaveCount(0)
    await expect(page.locator('.context-menu button', { hasText: 'Delete session' })).toHaveCount(0)
  })

  test('backend rejects upserting a tab for a non-existent item', async ({ request }) => {
    // Defense-in-depth check on the backend guard. The frontend should
    // never POST one of these, but if it does (or if any external
    // client tries), the server must refuse rather than silently store
    // an orphan row.
    const { auth } = await authenticate(request)

    const sessionRes = await request.post('/api/me/tabs', {
      headers: auth,
      data: { item_type: 'session', item_id: 'does-not-exist' },
    })
    expect(sessionRes.status()).toBe(404)

    const projectRes = await request.post('/api/me/tabs', {
      headers: auth,
      data: { item_type: 'project', item_id: 'does-not-exist' },
    })
    expect(projectRes.status()).toBe(404)
  })
})
