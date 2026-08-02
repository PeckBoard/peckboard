import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e test: SubagentTranscript must stream a running child session's
 * events over the WS per-session subscription instead of polling
 * `/api/sessions/{id}/events` on a 5s interval. One HTTP fetch backfills
 * on expand; everything after that arrives as WS pushes.
 *
 * Setup mirrors subagent-open-session.spec.ts: `mock:subagent` on the
 * parent emits a spawn_subagent tool card pointing at a real child
 * session, which we then drive with `mock:happy-path`.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
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

test('subagent transcript streams live over WS instead of polling', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, auth } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-subagent-live-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'e2e-subagent-live', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const childRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'subagent child', folder_id: folder.id },
  })
  expect(childRes.ok(), `create child session failed: ${await childRes.text()}`).toBeTruthy()
  const child = (await childRes.json()) as { id: string }

  const parentRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'subagent parent', folder_id: folder.id },
  })
  expect(parentRes.ok(), `create parent session failed: ${await parentRes.text()}`).toBeTruthy()
  const parent = (await parentRes.json()) as { id: string }

  const sendRes = await request.post(`/api/sessions/${parent.id}/message`, {
    headers: auth,
    data: { text: child.id, model: 'mock:subagent' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  // Count every request that hits the child's events-backfill endpoint so
  // we can prove there's exactly one — the old code refetched every 5s.
  let eventsFetchCount = 0
  await page.route(`**/api/sessions/${child.id}/events*`, async (route) => {
    eventsFetchCount += 1
    await route.continue()
  })

  await loadAt(page, token, `/sessions/${parent.id}`)

  const toggle = page.locator('.subagent-toggle')
  await expect(toggle).toBeVisible({ timeout: 15_000 })
  await toggle.click()

  await expect.poll(() => eventsFetchCount, { timeout: 10_000 }).toBe(1)

  // Drive the child session's own agent turn AFTER the backfill fetch —
  // every row that appears from here on must come from the WS push.
  const childMsgRes = await request.post(`/api/sessions/${child.id}/message`, {
    headers: auth,
    data: { text: 'go', model: 'mock:happy-path' },
  })
  expect(childMsgRes.ok(), `child message failed: ${await childMsgRes.text()}`).toBeTruthy()

  // Bash's row shows the actual command line, not the tool name.
  await expect(page.locator('.subagent-row-tool', { hasText: 'echo hello' })).toBeVisible({
    timeout: 10_000,
  })
  await expect(page.locator('.subagent-row-text', { hasText: 'Done.' })).toBeVisible({
    timeout: 10_000,
  })

  // Wait past the old 5s poll interval; the fetch count must still be 1.
  await page.waitForTimeout(6_000)
  expect(eventsFetchCount, 'no periodic re-poll after the initial backfill').toBe(1)
})
