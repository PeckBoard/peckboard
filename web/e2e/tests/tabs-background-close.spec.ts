import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E for the fix: closing a background (non-visible) tab chip must not
 * navigate away from the currently-visible view.
 *
 * Flows covered:
 *   (a) Close a background project chip while viewing a session →
 *       view/URL unchanged, session chip still active, project chip
 *       gone locally and server-side.
 *   (b) Re-activating that project afterwards stores a fresh tab
 *       (edge-guard re-armed correctly).
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
    await request.delete(`/api/me/tabs/${t.item_type}/${t.item_id}`, { headers: auth })
  }
}

async function seedSession(
  request: APIRequestContext,
  auth: Record<string, string>,
  name: string,
): Promise<string> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-bgclose-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'e2e-bgclose', path: folderPath },
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

async function seedProject(
  request: APIRequestContext,
  auth: Record<string, string>,
  name: string,
): Promise<string> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-bgclose-proj-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'e2e-bgclose-proj', path: folderPath },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name,
      folder_id: folder.id,
      worker_count: 1,
      model: 'mock:happy-path',
      workflow: 'task',
    },
  })
  expect(projectRes.ok()).toBeTruthy()
  return ((await projectRes.json()) as { id: string }).id
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })
}

test.describe('tabs-background-close', () => {
  test('closing a background project chip while viewing a session leaves view/URL unchanged', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)

    const sessionId = await seedSession(request, auth, 'active-session')
    const projectId = await seedProject(request, auth, 'bg-proj')

    // Start on the project view so the project tab opens and becomes active.
    await loadAt(page, token, `/projects/${projectId}`)
    const projectTab = page.locator('.tab-opened', { hasText: 'bg-proj' })
    await expect(projectTab).toBeVisible({ timeout: 5_000 })
    await expect(projectTab).toHaveClass(/tab-active/)

    // SPA-navigate to the session — activeProjectId stays set in the store
    // (background chip), view switches to sessions.
    await page.evaluate((id) => {
      history.pushState(null, '', `/sessions/${id}`)
      window.dispatchEvent(new PopStateEvent('popstate'))
    }, sessionId)

    const sessionTab = page.locator('.tab-opened', { hasText: 'active-session' })
    await expect(sessionTab).toHaveClass(/tab-active/, { timeout: 5_000 })
    await expect(projectTab).not.toHaveClass(/tab-active/)

    // Close the project chip via context menu — it is now a background chip.
    await projectTab.click({ button: 'right' })
    const closeBtn = page.locator('.context-menu button', { hasText: 'Close tab' })
    await expect(closeBtn).toBeVisible()
    await closeBtn.click()

    await expect(projectTab).toHaveCount(0)

    // (a) View/URL must remain on the session — no navigation side-effect.
    await expect(page).toHaveURL(new RegExp(`/sessions/${sessionId}`))
    await expect(sessionTab).toHaveClass(/tab-active/)

    // Server-side: project tab must be gone (tiny wait for DELETE to flush).
    await page.waitForTimeout(200)
    const tabsRes = await request.get('/api/me/tabs', { headers: auth })
    expect(tabsRes.ok()).toBeTruthy()
    const tabList = (await tabsRes.json()) as Array<{ item_type: string; item_id: string }>
    expect(
      tabList.find((t) => t.item_type === 'project' && t.item_id === projectId),
      `project tab should be gone server-side; got: ${JSON.stringify(tabList)}`,
    ).toBeUndefined()
  })

  test('re-activating a previously closed background project chip stores a fresh tab', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)

    const sessionId = await seedSession(request, auth, 'active-session-2')
    const projectId = await seedProject(request, auth, 'bg-proj-reopen')

    // Open project → switch to session (project becomes background) → close project chip.
    await loadAt(page, token, `/projects/${projectId}`)
    const projectTab = page.locator('.tab-opened', { hasText: 'bg-proj-reopen' })
    await expect(projectTab).toBeVisible({ timeout: 5_000 })
    await expect(projectTab).toHaveClass(/tab-active/)

    await page.evaluate((id) => {
      history.pushState(null, '', `/sessions/${id}`)
      window.dispatchEvent(new PopStateEvent('popstate'))
    }, sessionId)

    const sessionTab = page.locator('.tab-opened', { hasText: 'active-session-2' })
    await expect(sessionTab).toHaveClass(/tab-active/, { timeout: 5_000 })

    await projectTab.click({ button: 'right' })
    await expect(page.locator('.context-menu button', { hasText: 'Close tab' })).toBeVisible()
    await page.locator('.context-menu button', { hasText: 'Close tab' }).click()
    await expect(projectTab).toHaveCount(0)

    // Confirm it's gone server-side before re-activating.
    await page.waitForTimeout(200)
    const tabsBefore = await request.get('/api/me/tabs', { headers: auth })
    expect(tabsBefore.ok()).toBeTruthy()
    const listBefore = (await tabsBefore.json()) as Array<{ item_type: string; item_id: string }>
    expect(
      listBefore.find((t) => t.item_type === 'project' && t.item_id === projectId),
    ).toBeUndefined()

    // (b) Re-activate the project: chip must reappear and be stored server-side.
    await page.evaluate((id) => {
      history.pushState(null, '', `/projects/${id}`)
      window.dispatchEvent(new PopStateEvent('popstate'))
    }, projectId)

    const reopenedTab = page.locator('.tab-opened', { hasText: 'bg-proj-reopen' })
    await expect(reopenedTab).toBeVisible({ timeout: 5_000 })
    await expect(reopenedTab).toHaveClass(/tab-active/)

    await page.waitForTimeout(200)
    const tabsAfter = await request.get('/api/me/tabs', { headers: auth })
    expect(tabsAfter.ok()).toBeTruthy()
    const listAfter = (await tabsAfter.json()) as Array<{ item_type: string; item_id: string }>
    expect(
      listAfter.find((t) => t.item_type === 'project' && t.item_id === projectId),
      `project tab should be present server-side after re-activation; got: ${JSON.stringify(listAfter)}`,
    ).toBeDefined()
  })
})
