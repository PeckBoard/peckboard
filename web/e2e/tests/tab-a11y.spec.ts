import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E for the app shell's semantic structure.
 *
 * Flows covered:
 *   1. The tab strip is a real `tablist` — `role=tab` children with
 *      posinset/setsize/aria-controls, and the whole strip is ONE Tab stop
 *      (close buttons are out of the Tab order).
 *   2. Left/Right/Home/End move focus between chips and the roving
 *      `tabindex=0` follows the focused chip.
 *   3. Each view has exactly one `h1`, the rail is a named landmark, and
 *      the shared list exposes `list` / `listitem`.
 *   4. Small controls clear the WCAG 2.5.8 24x24 target size on a phone
 *      viewport (`.tab-close` via its transparent hit area, and the
 *      multi-select checkbox via its wrapping label).
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

let cachedAuth: { token: string; auth: Record<string, string> } | null = null

/** Authenticate once per spec file — the rate limiter sees every request
 *  from 127.0.0.1 as one client. */
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

async function seedFolderAndSession(
  request: APIRequestContext,
  auth: Record<string, string>,
  sessionName: string,
): Promise<{ sessionId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-taba11y-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'e2e-tab-a11y', path: folderPath },
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

/** Clear the server-side tab list so a test starts from a clean slate. */
async function clearTabs(request: APIRequestContext, auth: Record<string, string>) {
  const res = await request.get('/api/me/tabs', { headers: auth })
  if (!res.ok()) return
  const tabs = (await res.json()) as Array<{ item_type: string; item_id: string }>
  for (const t of tabs) {
    await request.delete(`/api/me/tabs/${t.item_type}/${t.item_id}`, { headers: auth })
  }
}

/** `role` + `data-tab-key` of whatever currently has focus. */
async function focusInfo(page: Page) {
  return page.evaluate(() => {
    const el = document.activeElement as HTMLElement | null
    return {
      role: el?.getAttribute('role') ?? null,
      key: el?.dataset.tabKey ?? null,
      cls: el?.className ?? null,
    }
  })
}

test.describe('tab strip semantics', () => {
  test('renders a labelled tablist whose chips are the only Tab stop', async ({
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
    await page.evaluate((id) => {
      history.pushState(null, '', `/sessions/${id}`)
      window.dispatchEvent(new PopStateEvent('popstate'))
    }, idB)

    const tablist = page.locator('[role="tablist"]')
    await expect(tablist).toHaveCount(1)
    await expect(tablist).toHaveAttribute('aria-label', 'Open tabs')

    // Chips are `tab` children of the tablist — the wrapper in between is
    // `role="presentation"`, so AT still reports "tab N of M".
    const tabs = tablist.locator('[role="tab"]')
    await expect(tabs).toHaveCount(2)
    await expect(tabs.first()).toHaveAttribute('aria-posinset', '1')
    await expect(tabs.first()).toHaveAttribute('aria-setsize', '2')
    await expect(tabs.nth(1)).toHaveAttribute('aria-posinset', '2')
    await expect(tabs.first()).toHaveAttribute('aria-controls', 'view-panel')
    await expect(page.locator('#view-panel')).toHaveAttribute('role', 'tabpanel')
    await expect(page.locator('.tab-wrap').first()).toHaveAttribute('role', 'presentation')

    // The `+` button must NOT be inside the tablist (a tablist may not own
    // non-tab children).
    await expect(tablist.locator('.tab-new')).toHaveCount(0)
    await expect(page.locator('.tabbar > .tab-new')).toHaveCount(1)

    // Exactly one chip is tabbable, and it's the active one.
    await expect(page.locator('[role="tab"][tabindex="0"]')).toHaveCount(1)
    await expect(page.locator('[role="tab"][tabindex="0"]')).toHaveClass(/tab-active/)
    // Close buttons are out of the Tab order.
    await expect(page.locator('.tab-close[tabindex="-1"]')).toHaveCount(2)

    // Tabbing off the roving chip leaves the strip entirely — one Tab stop
    // for two tabs and two close buttons.
    await page.locator('[role="tab"][tabindex="0"]').focus()
    expect((await focusInfo(page)).role).toBe('tab')
    await page.keyboard.press('Tab')
    const after = await focusInfo(page)
    expect(after.role).not.toBe('tab')
    expect(after.cls).not.toContain('tab-close')
  })

  test('arrow keys move between tabs and the roving tabindex follows', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)
    const { sessionId: idA } = await seedFolderAndSession(request, auth, 'gamma')
    const { sessionId: idB } = await seedFolderAndSession(request, auth, 'delta')

    await loadAt(page, token, `/sessions/${idA}`)
    await page.evaluate((id) => {
      history.pushState(null, '', `/sessions/${id}`)
      window.dispatchEvent(new PopStateEvent('popstate'))
    }, idB)
    await expect(page.locator('[role="tab"]')).toHaveCount(2)

    const first = page.locator('[role="tab"]').first()
    const second = page.locator('[role="tab"]').nth(1)
    const firstKey = await first.getAttribute('data-tab-key')
    const secondKey = await second.getAttribute('data-tab-key')

    await first.focus()
    await page.keyboard.press('ArrowRight')
    expect((await focusInfo(page)).key).toBe(secondKey)
    // The roving tabindex moved with focus: still exactly one tabbable chip.
    await expect(page.locator('[role="tab"][tabindex="0"]')).toHaveCount(1)
    await expect(second).toHaveAttribute('tabindex', '0')
    await expect(first).toHaveAttribute('tabindex', '-1')

    // Wraps around at the end, and Home/End jump to the edges.
    await page.keyboard.press('ArrowRight')
    expect((await focusInfo(page)).key).toBe(firstKey)
    await page.keyboard.press('End')
    expect((await focusInfo(page)).key).toBe(secondKey)
    await page.keyboard.press('Home')
    expect((await focusInfo(page)).key).toBe(firstKey)
    await page.keyboard.press('ArrowLeft')
    expect((await focusInfo(page)).key).toBe(secondKey)

    // Arrows move focus only — activation stays manual, so the active tab
    // (and therefore the open session) has not changed.
    await expect(page.locator('.tab-active')).toHaveCount(1)
    await expect(page.locator('.tab-opened.tab-active')).toContainText('delta')
  })

  test('each view has one h1, the rail is a named landmark, rows are a list', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)
    const { sessionId } = await seedFolderAndSession(request, auth, 'epsilon')

    await loadAt(page, token, '/sessions')
    await expect(page.locator('nav.rail')).toHaveAttribute('aria-label', 'Primary')
    await expect(page.locator('h1')).toHaveCount(1)
    await expect(page.locator('h1')).toHaveText('Sessions')
    // The shared List exposes list semantics without `ul`/`li`.
    await expect(page.locator('[role="list"]')).toHaveCount(1)
    expect(await page.locator('[role="list"] > [role="listitem"]').count()).toBeGreaterThan(0)

    await page.evaluate((id) => {
      history.pushState(null, '', `/sessions/${id}`)
      window.dispatchEvent(new PopStateEvent('popstate'))
    }, sessionId)
    await expect(page.locator('.chat-container')).toBeVisible({ timeout: 5_000 })
    await expect(page.locator('h1')).toHaveCount(1)
    await expect(page.locator('h1')).toHaveText('epsilon')
  })
})

test.describe('touch targets on a phone viewport', () => {
  test.use({ viewport: { width: 390, height: 844 } })

  test('tab close and row checkbox clear 24x24', async ({ request, page, baseURL }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)
    const { sessionId } = await seedFolderAndSession(request, auth, 'zeta')

    await loadAt(page, token, `/sessions/${sessionId}`)
    const close = page.locator('.tab-close').first()
    await expect(close).toBeVisible()
    const box = (await close.boundingBox())!
    expect(box).toBeTruthy()

    // The visible glyph may be smaller than 24px; what has to clear 24x24 is
    // the hit area, so probe just outside each edge of the visible box and
    // check the hit test still lands on the close button.
    const probes: Array<[number, number]> = [
      [box.x - 1.5, box.y + box.height / 2],
      [box.x + box.width + 1.5, box.y + box.height / 2],
      [box.x + box.width / 2, box.y - 1.5],
      [box.x + box.width / 2, box.y + box.height + 1.5],
    ]
    for (const [x, y] of probes) {
      const onClose = await page.evaluate(
        ([px, py]) =>
          !!(document.elementFromPoint(px, py) as HTMLElement | null)?.closest('.tab-close'),
        [x, y],
      )
      expect(onClose, `hit test at ${x},${y} should land on .tab-close`).toBeTruthy()
    }
    expect(box.width + 3).toBeGreaterThanOrEqual(24)
    expect(box.height + 3).toBeGreaterThanOrEqual(24)

    // The multi-select checkbox keeps its 16px box inside a 24px label.
    await page.evaluate(() => {
      history.pushState(null, '', '/sessions')
      window.dispatchEvent(new PopStateEvent('popstate'))
    })
    const hit = page.locator('.list-view-select-hit').first()
    await expect(hit).toBeVisible({ timeout: 5_000 })
    const hitBox = (await hit.boundingBox())!
    expect(hitBox.width).toBeGreaterThanOrEqual(24)
    expect(hitBox.height).toBeGreaterThanOrEqual(24)
  })
})
