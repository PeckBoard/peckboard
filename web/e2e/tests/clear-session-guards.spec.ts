import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E for the CLEAR-session UI guard for repeating-task sessions.
 *
 * Backend coverage is in `tests/clear_session_guards.rs` (worker AND
 * repeating-task sessions: POST /clear → 409). This spec pins the
 * matching UI behaviour for the case we can spawn from the test
 * environment without an orchestrator/agent — a session kicked off by
 * a force-run repeating task. The relevant invariants:
 *
 *   1. The chat-toolbar 3-dot menu hides "Clear session" when the
 *      session has a `repeating_task_id` set, but still offers
 *      "Delete" (the repeating-task run history can be pruned by
 *      deleting individual runs).
 *   2. The tab-strip context menu mirrors the chat-toolbar — same
 *      labels, same hide rules, so the user sees a consistent menu
 *      across every surface.
 *   3. Direct POST /api/sessions/:id/clear on such a session returns
 *      409 with a "repeating task run" reason, even if the UI is
 *      bypassed.
 *
 * Worker-session menus are exercised by the existing worker-session
 * coverage; this spec stays focused on the repeating-task case so its
 * setup stays simple.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok()).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
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

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })
}

/** Seed a folder + repeating task that uses the mock provider, force-
 *  run it, and return the spawned session id. Polls for the session
 *  because the spawn is async on the server. */
async function spawnRepeatingTaskSession(
  request: APIRequestContext,
  auth: Record<string, string>,
  taskName: string,
): Promise<{ taskId: string; sessionId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-clear-${Date.now()}-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-clear-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const taskRes = await request.post('/api/repeating-tasks', {
    headers: { ...auth, 'Content-Type': 'application/json' },
    data: {
      name: taskName,
      description: '',
      folder_id: folder.id,
      prompt: 'do thing',
      schedule_kind: 'interval',
      schedule_value: { minutes: 60 },
      model: 'mock:happy-path',
    },
  })
  expect(taskRes.ok()).toBeTruthy()
  const task = (await taskRes.json()) as { id: string }

  const runRes = await request.post(`/api/repeating-tasks/${task.id}/run`, { headers: auth })
  expect(runRes.ok()).toBeTruthy()

  // The run spawns a session asynchronously. Poll the per-task session
  // list rather than racing the broadcaster: the existing
  // repeating-tasks-ui spec does the same.
  let sessionId: string | null = null
  for (let i = 0; i < 30 && !sessionId; i++) {
    const listRes = await request.get(`/api/repeating-tasks/${task.id}/sessions`, {
      headers: auth,
    })
    if (listRes.ok()) {
      const sessions = (await listRes.json()) as Array<{ id: string }>
      if (sessions.length > 0) sessionId = sessions[0].id
    }
    if (!sessionId) await new Promise((r) => setTimeout(r, 100))
  }
  expect(sessionId, 'force-run never produced a session').toBeTruthy()
  return { taskId: task.id, sessionId: sessionId! }
}

test.describe('CLEAR guard for repeating-task sessions', () => {
  test('chat-toolbar 3-dot menu hides "Clear session" but keeps "Delete"', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)
    const { sessionId } = await spawnRepeatingTaskSession(
      request,
      auth,
      `clear-guard-${Date.now()}`,
    )

    await loadAt(page, token, `/sessions/${sessionId}`)
    // Wait for ChatView to mount before opening the menu — otherwise
    // the menu can briefly render before sessionDetail loads, with the
    // `hidden` prop computed from `is_worker = false / repeating_task_id
    // = null` (the default fallback for an unresolved fetch).
    await expect(page.locator('.chat-toolbar')).toBeVisible({ timeout: 10_000 })

    // Open the 3-dot session menu.
    await page.getByTestId('chat-toolbar-menu').click()

    // "Clear session" must NOT appear. Use the test id we wired in
    // ChatView so a label change in the future doesn't silently make
    // this assertion vacuous.
    await expect(page.getByTestId('chat-menu-clear')).toHaveCount(0)

    // "Delete" must still appear — repeating-task sessions delete fine
    // (the run leaves the task's history, the schedule keeps firing).
    await expect(page.getByTestId('chat-menu-delete')).toBeVisible()

    // "Terminate agent" must still appear — terminating the run is
    // distinct from clearing its transcript.
    await expect(page.getByTestId('chat-toolbar-terminate')).toBeVisible()
  })

  test('tab right-click menu hides "Clear session" for a repeating-task session', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)
    const { sessionId } = await spawnRepeatingTaskSession(
      request,
      auth,
      `clear-guard-tab-${Date.now()}`,
    )

    await loadAt(page, token, `/sessions/${sessionId}`)
    await expect(page.locator('.chat-toolbar')).toBeVisible({ timeout: 10_000 })

    // The session has its tab chip on the strip — find it and open the
    // right-click context menu.
    const tab = page.locator('.tab-opened.tab-active')
    await expect(tab).toBeVisible()
    await tab.click({ button: 'right' })

    // No "Clear session" entry, but Delete is still there.
    await expect(page.locator('.context-menu button', { hasText: /^Clear session$/ })).toHaveCount(
      0,
    )
    await expect(
      page.locator('.context-menu button', { hasText: /^Delete session$/ }),
    ).toBeVisible()
  })

  test('POST /api/sessions/:id/clear returns 409 on a repeating-task session', async ({
    request,
  }) => {
    // Defence-in-depth: even if a stale UI bypasses the menu hide,
    // the backend refuses the clear with a typed reason. Mirrors the
    // backend Rust test but pinned through the actual HTTP boundary.
    const { auth } = await authenticate(request)
    const { sessionId } = await spawnRepeatingTaskSession(
      request,
      auth,
      `clear-guard-api-${Date.now()}`,
    )

    const res = await request.post(`/api/sessions/${sessionId}/clear`, { headers: auth })
    expect(res.status()).toBe(409)
    const body = (await res.json()) as { error?: string }
    expect(body.error ?? '').toMatch(/repeating task/i)
  })
})
