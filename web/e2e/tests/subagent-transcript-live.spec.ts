import { test, expect, type APIRequestContext, type Page } from '../harness'
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
    // Auto subagent panes would mount the child's own ChatView, whose
    // backfill fetch would be counted below; this spec is about the inline
    // transcript alone.
    localStorage.setItem('peckboard.subagentPanes', 'off')
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

/**
 * Regression: SubagentTranscript used to treat the child's own `agent-end`
 * as "the subagent is finished", so a child that ends its first turn while
 * a `run_background` task is still in flight (spawn_subagent's documented
 * multi-turn shape — see subagent-auto-panes.spec.ts) had its spinner
 * vanish and its live WS subscription torn down before the real result
 * (posted on the child's second turn, once the task wakes it) ever
 * arrived. The fix reads the session's `subagent_completed_at` (backfilled
 * on expand, then kept live via the same `session-updated` broadcast
 * SessionWorkspace's auto-panes already rely on) instead.
 */
test('subagent transcript stays running until the server stamps completion, not on agent-end', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, auth } = await authenticate(request)

  // run_background shares run_command's approval gate; a subagent session
  // is not a worker, so it would prompt. Bypass for the test.
  const priorRes = await request.get('/api/settings/tool-permissions', { headers: auth })
  expect(priorRes.ok()).toBeTruthy()
  const priorBypass = ((await priorRes.json()) as { bypass: boolean }).bypass
  const bypassOn = await request.put('/api/settings/tool-permissions', {
    headers: auth,
    data: { bypass: true },
  })
  expect(bypassOn.ok(), `enable bypass failed: ${await bypassOn.text()}`).toBeTruthy()

  try {
    const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-subagent-live-multi-'))
    const folderRes = await request.post('/api/folders', {
      headers: auth,
      data: { name: 'e2e-subagent-live-multi', path: folderPath },
    })
    expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
    const folder = (await folderRes.json()) as { id: string }

    const parentRes = await request.post('/api/sessions', {
      headers: auth,
      data: { name: 'subagent parent multi', folder_id: folder.id },
    })
    expect(parentRes.ok(), `create parent session failed: ${await parentRes.text()}`).toBeTruthy()
    const parent = (await parentRes.json()) as { id: string }

    // Spawn a REAL child (parent_session_id set, so the server-side
    // completion claim applies) whose model starts a real `run_background`
    // task and ends its own first turn before the task exits.
    const call = {
      tool: 'spawn_subagent',
      args: { name: 'bg', prompt: 'Do the thing.', model: 'mock:subagent-bg-child' },
    }
    const block = '```mcp\n' + JSON.stringify(call) + '\n```'
    const spawnRes = await request.post(`/api/sessions/${parent.id}/message`, {
      headers: auth,
      data: { text: block, model: 'mock:mcp' },
    })
    expect(spawnRes.ok(), `spawn failed: ${await spawnRes.text()}`).toBeTruthy()

    let childId = ''
    await expect
      .poll(
        async () => {
          const res = await request.get(`/api/sessions/${parent.id}/children`, { headers: auth })
          const list = (await res.json()) as { id: string }[]
          if (list[0]) childId = list[0].id
          return list.length
        },
        { timeout: 15_000 },
      )
      .toBe(1)

    await loadAt(page, token, `/sessions/${parent.id}`)

    const toggle = page.locator('.subagent-toggle')
    await expect(toggle).toBeVisible({ timeout: 15_000 })
    await toggle.click()

    // Drive the child's first turn: it launches `sleep 3` in the
    // background and ends its OWN turn with the task still running.
    const childMsgRes = await request.post(`/api/sessions/${childId}/message`, {
      headers: auth,
      data: { text: 'start', model: 'mock:subagent-bg-child' },
    })
    expect(childMsgRes.ok(), `child message failed: ${await childMsgRes.text()}`).toBeTruthy()

    await expect(
      page.locator('.subagent-row-text', { hasText: 'child launched background work' }),
    ).toBeVisible({ timeout: 10_000 })

    // The child's own turn just ended (agent-end) but the subagent is NOT
    // done — its background task is still running. The spinner must still
    // show: this is exactly the state the old `agent-end`-only check got
    // wrong.
    await expect(page.locator('.subagent-toggle .tool-spinner')).toBeVisible()

    // The task exits, wakes the child, and its real final reply lands live
    // (the transcript must still be subscribed to see this).
    await expect(page.locator('.subagent-row-text', { hasText: 'CHILD FINAL' })).toBeVisible({
      timeout: 20_000,
    })

    // Only now, once the server has actually stamped `subagent_completed_at`,
    // must the spinner disappear.
    await expect(page.locator('.subagent-toggle .tool-spinner')).toHaveCount(0)
  } finally {
    await request.put('/api/settings/tool-permissions', {
      headers: auth,
      data: { bypass: priorBypass },
    })
  }
})
