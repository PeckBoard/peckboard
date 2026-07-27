import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E for the tab strip's trailing controls with far more tabs than fit.
 *
 * The strip (`.tab-strip`) scrolls horizontally; the `+` button and the
 * `»` overflow trigger are siblings OUTSIDE it, so neither can scroll off
 * screen. Covered here, on a desktop and a phone viewport:
 *   1. `+` stays in the viewport, stays the last child of the bar, and
 *      still opens the New Session modal with 14 tabs open.
 *   2. `»` appears only when the strip is actually clipping chips, and
 *      lists exactly those chips.
 *   3. Picking a chip from `»` activates it AND scrolls it into view.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
/** Comfortably past the ~8 tabs that fill a desktop strip. */
const TAB_COUNT = 14

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

/** Clear the server-side tab list so a test starts from a clean slate. */
async function clearTabs(request: APIRequestContext, auth: Record<string, string>) {
  const res = await request.get('/api/me/tabs', { headers: auth })
  if (!res.ok()) return
  const tabs = (await res.json()) as Array<{ item_type: string; item_id: string }>
  for (const t of tabs) {
    await request.delete(`/api/me/tabs/${t.item_type}/${t.item_id}`, { headers: auth })
  }
}

/** Seed one folder + TAB_COUNT sessions, each opened as a tab. Names are
 *  long enough that a dozen chips can't fit any viewport we test. */
async function seedTabs(
  request: APIRequestContext,
  auth: Record<string, string>,
): Promise<{ ids: string[] }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-taboverflow-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'e2e-tab-overflow', path: folderPath },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const ids: string[] = []
  for (let i = 1; i <= TAB_COUNT; i++) {
    const sessionRes = await request.post('/api/sessions', {
      headers: auth,
      data: { name: `Overflow session ${String(i).padStart(2, '0')}`, folder_id: folder.id },
    })
    expect(sessionRes.ok()).toBeTruthy()
    const session = (await sessionRes.json()) as { id: string }
    ids.push(session.id)
    const tabRes = await request.post('/api/me/tabs', {
      headers: auth,
      data: { item_type: 'session', item_id: session.id },
    })
    expect(tabRes.ok()).toBeTruthy()
  }
  return { ids }
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })
}

/** Labels of the chips the strip is currently clipping, measured the same
 *  way TabBar does (chip rect vs the scroller's rect). */
async function clippedLabels(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const strip = document.querySelector('.tab-strip')
    if (!strip) return []
    const box = strip.getBoundingClientRect()
    return Array.from(strip.querySelectorAll('.tab-wrap'))
      .filter((el) => {
        const r = el.getBoundingClientRect()
        return r.left < box.left - 1 || r.right > box.right + 1
      })
      .map((el) => el.querySelector('.tab-label')?.textContent ?? '')
  })
}

for (const vp of [
  { name: 'desktop', width: 1280, height: 720 },
  { name: 'mobile', width: 390, height: 844 },
]) {
  test(`${vp.name}: with ${TAB_COUNT} tabs, "+" stays reachable and "»" jumps to an off-screen tab`, async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL, 'baseURL configured').toBeTruthy()
    await page.setViewportSize({ width: vp.width, height: vp.height })

    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)
    const { ids } = await seedTabs(request, auth)

    await loadAt(page, token, `/sessions/${ids[0]}`)
    await expect(page.locator('[role="tab"]')).toHaveCount(TAB_COUNT)

    // 1. The strip really is clipping chips at this width.
    const clipped = await clippedLabels(page)
    expect(clipped.length, 'the strip should be clipping chips').toBeGreaterThan(0)

    // 2. `+` is still on screen and still the last child of the bar — it
    //    lives outside the scroller, so no amount of tabs can push it off.
    const newBtn = page.locator('.tab-new')
    await expect(newBtn).toBeVisible()
    await expect(newBtn).toBeInViewport()
    await expect(page.locator('.tabbar > *').last()).toHaveClass(/tab-new/)

    // 3. The overflow trigger lists exactly the clipped chips.
    const trigger = page.getByTestId('tab-overflow')
    await expect(trigger).toBeVisible()
    await expect(trigger).toBeInViewport()
    await trigger.click()
    const menu = page.locator('.dropdown-menu[role="menu"]')
    await expect(menu).toBeVisible()
    await expect(menu.locator('.dropdown-item')).toHaveCount(clipped.length)

    // 4. Picking one activates it and scrolls it into view.
    const target = clipped[clipped.length - 1]
    await menu.getByRole('menuitem', { name: target, exact: true }).click()
    await expect(menu).toHaveCount(0)
    await expect(page.locator('.tab-wrap:has(.tab-active) .tab-label')).toHaveText(target)
    await expect
      .poll(async () => await clippedLabels(page), {
        message: 'the picked tab should have been scrolled into view',
      })
      .not.toContain(target)

    // 5. `+` still opens the New Session modal from this crowded strip.
    await newBtn.click()
    await expect(page.locator('.modal').first()).toBeVisible({ timeout: 5_000 })
  })
}
