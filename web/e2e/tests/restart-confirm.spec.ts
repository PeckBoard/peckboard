import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Restart guard (Settings → Server → "Restart server"):
 *
 *  - With nothing running, the click restarts straight away — no dialog.
 *  - With a session mid-turn (`mock:block`), the click first shows what a
 *    restart would interrupt. Cancel restarts nothing.
 *  - "Restart when idle" parks the restart on the server and shows the
 *    app-wide pending banner; its Cancel drops it again.
 *
 * The immediate restart POST is intercepted with `page.route` so the
 * shared test server is never actually re-exec'd. The idle restart does
 * reach the server, and is always cancelled before the blocking turn ends.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

async function activityTotal(request: APIRequestContext, auth: Record<string, string>) {
  const res = await request.get('/api/admin/activity', { headers: auth })
  expect(res.ok(), `activity failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { total: number }).total
}

async function openServerSettings(page: Page, token: string) {
  await loadAt(page, token, '/settings')
  await page.locator('[data-testid="settings-nav-server"]').click()
  const btn = page.locator('[data-testid="server-restart"]')
  await expect(btn).toBeVisible({ timeout: 10_000 })
  return btn
}

/** Count immediate-restart POSTs and answer them without restarting. */
async function stubImmediateRestart(page: Page) {
  const calls = { now: 0 }
  await page.route('**/api/admin/restart', async (route) => {
    if (route.request().method() !== 'POST') return route.fallback()
    calls.now += 1
    await route.fulfill({ json: { ok: true, restarting: true } })
  })
  // The component reloads once the server answers again; keep it waiting
  // so the page stays put for the assertions.
  await page.route('**/api/health', (route) => route.fulfill({ status: 503, body: '' }))
  return calls
}

test('with nothing running, Restart server restarts without a dialog', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  await expect.poll(() => activityTotal(request, auth), { timeout: 15_000 }).toBe(0)

  const calls = await stubImmediateRestart(page)
  const btn = await openServerSettings(page, token)
  await btn.click()

  await expect(page.locator('[data-testid="update-restarting"]')).toBeVisible()
  await expect(page.locator('[data-testid="restart-confirm"]')).toHaveCount(0)
  expect(calls.now).toBe(1)
})

test('a session mid-turn is listed before restarting; Cancel and idle-cancel restart nothing', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-restart-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-restart-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionName = `restart-guard-${Date.now()}`
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: sessionName, folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  const send = await request.post(`/api/sessions/${session.id}/message`, {
    headers: auth,
    data: { text: 'stay busy', model: 'mock:block' },
  })
  expect(send.ok(), `send failed: ${await send.text()}`).toBeTruthy()

  try {
    await expect.poll(() => activityTotal(request, auth), { timeout: 15_000 }).toBeGreaterThan(0)

    const calls = await stubImmediateRestart(page)
    const btn = await openServerSettings(page, token)
    const dialog = page.locator('[data-testid="restart-confirm"]')

    // The dialog names the running session; Cancel keeps the server up.
    await btn.click()
    await expect(dialog).toBeVisible()
    await expect(dialog).toContainText('would be interrupted')
    const group = dialog.locator('[data-testid="restart-group-sessions"]')
    await group.locator('summary').click()
    await expect(group).toContainText(sessionName)
    await dialog.locator('[data-testid="confirm-dialog-cancel"]').click()
    await expect(dialog).toHaveCount(0)
    expect(calls.now).toBe(0)
    const health = await request.get('/api/health')
    expect(health.ok()).toBeTruthy()

    // "Restart when idle" parks it server-side and shows the banner.
    await btn.click()
    await expect(dialog).toBeVisible()
    await dialog.locator('[data-testid="restart-confirm-idle"]').click()
    await expect(dialog).toHaveCount(0)
    const banner = page.locator('[data-testid="restart-pending-banner"]')
    await expect(banner).toBeVisible({ timeout: 10_000 })
    await expect(banner).toContainText('Restart pending')
    await expect(page.locator('[data-testid="restart-pending-remaining"]')).toContainText(
      'Waiting for',
    )

    // Cancel from the banner drops it on the server.
    await page.locator('[data-testid="restart-pending-cancel"]').click()
    await expect(banner).toHaveCount(0)
    const pending = await request.get('/api/admin/restart', { headers: auth })
    expect(((await pending.json()) as { pending: unknown }).pending).toBeNull()
    expect(calls.now).toBe(0)
  } finally {
    // Never let a parked restart outlive the blocking turn.
    await request.delete('/api/admin/restart', { headers: auth })
    await request.post(`/api/sessions/${session.id}/interrupt`, { headers: auth })
  }
})
