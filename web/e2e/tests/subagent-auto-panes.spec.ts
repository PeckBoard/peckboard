import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { DatabaseSync } from 'node:sqlite'

/**
 * Auto subagent panes follow a subagent's REAL lifetime, not its first
 * turn end:
 *
 *  1. A Claude-native Agent launched with `run_in_background` keeps its
 *     pane open past its tool-end and past the parent's settled turn, and
 *     closes only when the plugin's `background-task-settled` event names
 *     its tool_use_id (`mock:subagent-background`).
 *  2. A spawn_subagent child that started a `run_background` task is not
 *     reported to the parent while the task runs: no `[subagent … finished]`
 *     until the task exits, the pane stays open meanwhile, and the result
 *     is the child's post-wake reply (`mock:subagent-bg-child`).
 *  3. Auto mode opens a pane for EVERY active child — seven at once, no
 *     overflow chip — and closes each as it finishes (`mock:slow`, with
 *     the concurrent-subagent limit raised above the default of 5).
 *
 * Children are spawned for real through `spawn_subagent` (a `mock:mcp`
 * parent running the ```mcp blocks in its message), so the server-side
 * completion claim applies. The provider-side tool path does not drive the
 * child's first turn (that marker is handled on the HTTP `/mcp` route
 * only), so each spec sends the child's first message over REST.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type Auth = { token: string; auth: Record<string, string> }

type SessionRow = { id: string; name: string; subagent_completed_at?: string | null }

type SessionEvent = {
  seq: number
  kind: string
  data: { text?: string; source?: string; subtype?: string; detail?: Record<string, unknown> }
}

async function authenticate(request: APIRequestContext): Promise<Auth> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function createFolder(request: APIRequestContext, auth: Auth, name: string) {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-${name}-`))
  const res = await request.post('/api/folders', {
    headers: auth.auth,
    data: { name, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function createSession(
  request: APIRequestContext,
  auth: Auth,
  folderId: string,
  name: string,
  model?: string,
): Promise<string> {
  const res = await request.post('/api/sessions', {
    headers: auth.auth,
    data: { name, folder_id: folderId, ...(model ? { model } : {}) },
  })
  expect(res.ok(), `create session failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function send(
  request: APIRequestContext,
  auth: Auth,
  sessionId: string,
  text: string,
  model: string,
) {
  const res = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth.auth,
    data: { text, model },
  })
  expect(res.ok(), `send failed: ${await res.text()}`).toBeTruthy()
}

async function events(request: APIRequestContext, auth: Auth, sessionId: string) {
  const res = await request.get(`/api/sessions/${sessionId}/events?after_seq=0`, {
    headers: auth.auth,
  })
  expect(res.ok(), `list events failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as SessionEvent[]
}

async function agentTexts(request: APIRequestContext, auth: Auth, sessionId: string) {
  return (await events(request, auth, sessionId))
    .filter((e) => e.kind === 'agent-text')
    .map((e) => e.data.text ?? '')
}

/** Latest lifecycle event kind on a session's transcript. */
async function lastLifecycle(request: APIRequestContext, auth: Auth, sessionId: string) {
  const kinds = (await events(request, auth, sessionId))
    .filter((e) => e.kind === 'agent-start' || e.kind === 'agent-end')
    .map((e) => e.kind)
  return kinds[kinds.length - 1] ?? null
}

/** Subagent result reports delivered to a parent session. */
async function subagentReports(request: APIRequestContext, auth: Auth, parentId: string) {
  return (await events(request, auth, parentId)).filter(
    (e) => e.kind === 'user' && e.data.source === 'subagent-result',
  )
}

async function children(request: APIRequestContext, auth: Auth, parentId: string) {
  const res = await request.get(`/api/sessions/${parentId}/children`, { headers: auth.auth })
  expect(res.ok(), `list children failed: ${await res.text()}`).toBeTruthy()
  const body = (await res.json()) as SessionRow[] | { children?: SessionRow[] }
  return Array.isArray(body) ? body : (body.children ?? [])
}

/** Poll until the parent has `count` child sessions; returns them. */
async function waitForChildren(
  request: APIRequestContext,
  auth: Auth,
  parentId: string,
  count: number,
) {
  await expect
    .poll(async () => (await children(request, auth, parentId)).length, { timeout: 20_000 })
    .toBe(count)
  return children(request, auth, parentId)
}

/** A ```mcp block that spawns a real child through spawn_subagent. */
function spawnBlock(name: string, model: string) {
  const call = { tool: 'spawn_subagent', args: { name, prompt: 'Do the thing.', model } }
  return '```mcp\n' + JSON.stringify(call) + '\n```'
}

/** `run_background` shares `run_command`'s approval gate; a subagent
 *  session is not a worker, so it would prompt. Bypass for the test. */
async function withBypass(request: APIRequestContext, auth: Auth, body: () => Promise<void>) {
  const prior = await request.get('/api/settings/tool-permissions', { headers: auth.auth })
  expect(prior.ok()).toBeTruthy()
  const priorBypass = ((await prior.json()) as { bypass: boolean }).bypass
  const on = await request.put('/api/settings/tool-permissions', {
    headers: auth.auth,
    data: { bypass: true },
  })
  expect(on.ok(), `enable bypass failed: ${await on.text()}`).toBeTruthy()
  try {
    await body()
  } finally {
    await request.put('/api/settings/tool-permissions', {
      headers: auth.auth,
      data: { bypass: priorBypass },
    })
  }
}

/** The concurrent-subagent cap lives in the app settings store
 *  (`subagent_limits`, read fresh on every spawn) with no REST route, so
 *  write it straight into the server's SQLite file. `null` removes it. */
function setSubagentLimit(maxConcurrent: number | null) {
  const dataDir = process.env.PECKBOARD_E2E_DATA_DIR
  if (!dataDir) throw new Error('PECKBOARD_E2E_DATA_DIR is not set')
  const db = new DatabaseSync(path.join(dataDir, 'peckboard.db'))
  try {
    db.exec('PRAGMA busy_timeout = 5000')
    if (maxConcurrent === null) {
      db.prepare(
        `DELETE FROM plugin_data
          WHERE plugin_id = 'core.settings' AND collection = 'app' AND key = 'subagent_limits'`,
      ).run()
    } else {
      db.prepare(
        `INSERT INTO plugin_data (plugin_id, collection, key, data)
          VALUES ('core.settings', 'app', 'subagent_limits', ?)
          ON CONFLICT (plugin_id, collection, key)
          DO UPDATE SET data = excluded.data, updated_at = datetime('now')`,
      ).run(JSON.stringify({ max_concurrent: maxConcurrent }))
    }
  } finally {
    db.close()
  }
}

/** Open a session and wait until its WS subscription is on the wire, so a
 *  turn sent right after is seen live rather than missed mid-backfill. */
async function openSession(page: Page, token: string, sessionId: string) {
  const subscribed = new Promise<void>((resolve) => {
    page.on('websocket', (ws) => {
      ws.on('framesent', (frame) => {
        const text = typeof frame.payload === 'string' ? frame.payload : ''
        if (text.includes('"subscribe"') && text.includes(sessionId)) resolve()
      })
    })
  })
  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto(`/sessions/${sessionId}`)
  await subscribed
  await expect(page.getByTestId('session-workspace')).toBeVisible({ timeout: 15_000 })
  await page.waitForTimeout(300)
}

test.describe('auto subagent panes follow the subagent lifetime', () => {
  test('a native background subagent pane outlives the parent turn and closes on settle', async ({
    request,
    page,
  }) => {
    const auth = await authenticate(request)
    const folder = await createFolder(request, auth, 'autopane-native-bg')
    const parent = await createSession(request, auth, folder, 'native bg parent')

    await openSession(page, auth.token, parent)
    const workspace = page.getByTestId('session-workspace')
    const primary = page.locator('[data-pane-id="@primary"]')
    const pane = page.getByTestId('native-subagent-pane')
    const chip = page.getByTestId('subagent-overflow-chip')

    await send(request, auth, parent, 'explore in the background', 'mock:subagent-background')

    // Launch: the pane opens at once.
    await expect(pane).toBeVisible({ timeout: 15_000 })
    const nativePane = workspace.getByTestId('split-pane').filter({ has: pane })
    await expect(nativePane.getByTestId('split-pane-header')).toContainText('Background explore')

    // The parent's turn settles (text + linger) while the agent keeps
    // running: the pane must stay, and nothing sits in the overflow.
    await expect(primary).toContainText('Parent turn settled', { timeout: 15_000 })
    await expect(pane).toContainText('Background child is looking around', { timeout: 5_000 })
    await page.waitForTimeout(700)
    await expect(pane).toBeVisible()
    await expect(chip).toHaveCount(0)

    // The settle event names the launching tool_use_id: pane closes, the
    // finished subagent moves to the overflow.
    await expect(pane).toHaveCount(0, { timeout: 15_000 })
    await expect(chip).toHaveText('+1')
    await expect.poll(() => lastLifecycle(request, auth, parent)).toBe('agent-end')

    const settled = (await events(request, auth, parent)).find(
      (e) => e.kind === 'system' && e.data.subtype === 'background-task-settled',
    )
    expect(settled?.data.detail).toMatchObject({
      tool_use_id: 'toolu_native_bg_1',
      status: 'completed',
    })
  })

  test('a spawn_subagent child with a background task reports only once the task exits', async ({
    request,
    page,
  }) => {
    test.setTimeout(60_000)
    const auth = await authenticate(request)
    await withBypass(request, auth, async () => {
      const folder = await createFolder(request, auth, 'autopane-bg-child')
      const parent = await createSession(request, auth, folder, 'bg child parent', 'mock:mcp')

      await openSession(page, auth.token, parent)
      const workspace = page.getByTestId('session-workspace')

      await send(
        request,
        auth,
        parent,
        spawnBlock('bg child', 'mock:subagent-bg-child'),
        'mock:mcp',
      )
      const [child] = await waitForChildren(request, auth, parent, 1)
      expect(child.subagent_completed_at ?? null).toBeNull()
      const paneChild = workspace.locator(`[data-pane-id="${child.id}"]`)
      await expect(paneChild).toBeVisible({ timeout: 15_000 })

      // Drive the child's first turn: it starts `sleep 3` in the background
      // and ends the turn with the task still running.
      await send(request, auth, child.id, 'start', 'mock:subagent-bg-child')
      await expect
        .poll(() => agentTexts(request, auth, child.id), { timeout: 15_000 })
        .toContain('child launched background work')
      await expect
        .poll(() => lastLifecycle(request, auth, child.id), { timeout: 15_000 })
        .toBe('agent-end')

      // Turn ended, task running: not claimed, not reported, pane still open.
      await page.waitForTimeout(700)
      expect(await subagentReports(request, auth, parent)).toHaveLength(0)
      const midway = (await children(request, auth, parent)).find((s) => s.id === child.id)
      expect(midway?.subagent_completed_at ?? null).toBeNull()
      await expect(paneChild).toBeVisible()

      // Task exit → child woken → its post-wake reply is THE result.
      await expect
        .poll(() => subagentReports(request, auth, parent), { timeout: 20_000 })
        .toHaveLength(1)
      const [report] = await subagentReports(request, auth, parent)
      expect(report.data.text).toMatch(
        /^\[subagent "bg child" \([^)]+\) finished\]\n\nCHILD FINAL: background work done$/,
      )
      expect(report.data.text).not.toContain('child launched background work')

      // Claimed: stamped on the row, pane gone.
      await expect
        .poll(
          async () =>
            (await children(request, auth, parent)).find((s) => s.id === child.id)
              ?.subagent_completed_at ?? null,
          { timeout: 10_000 },
        )
        .not.toBeNull()
      await expect(paneChild).toHaveCount(0, { timeout: 15_000 })
      await expect(page.getByTestId('subagent-overflow-chip')).toHaveText('+1')
    })
  })

  test('auto mode opens a pane for every active child (seven) and closes each as it finishes', async ({
    request,
    page,
  }) => {
    test.setTimeout(90_000)
    await page.setViewportSize({ width: 1600, height: 1000 })
    const auth = await authenticate(request)
    setSubagentLimit(10)
    try {
      const folder = await createFolder(request, auth, 'autopane-seven')
      const parent = await createSession(request, auth, folder, 'seven parent', 'mock:mcp')

      await openSession(page, auth.token, parent)
      const workspace = page.getByTestId('session-workspace')
      const panes = workspace.getByTestId('split-pane')
      const chip = page.getByTestId('subagent-overflow-chip')

      const names = ['c1', 'c2', 'c3', 'c4', 'c5', 'c6', 'c7']
      await send(
        request,
        auth,
        parent,
        names.map((n) => spawnBlock(n, 'mock:slow')).join('\n'),
        'mock:mcp',
      )
      await expect
        .poll(() => agentTexts(request, auth, parent), { timeout: 20_000 })
        .toContain('ran 7/7 mcp block(s)')
      const kids = await waitForChildren(request, auth, parent, 7)
      const byName = new Map(kids.map((s) => [s.name.replace(/^sub: /, ''), s.id]))
      const short = names.slice(0, 4).map((n) => byName.get(n)!)
      const long = names.slice(4).map((n) => byName.get(n)!)

      // Run every child: four finish after ~4s, three after ~10s.
      for (const id of short) await send(request, auth, id, 'sleep:4', 'mock:slow')
      for (const id of long) await send(request, auth, id, 'sleep:10', 'mock:slow')

      // All seven active: seven child panes beside the parent, no overflow.
      await expect(panes).toHaveCount(8, { timeout: 15_000 })
      for (const id of [...short, ...long]) {
        await expect(workspace.locator(`[data-pane-id="${id}"]`)).toBeVisible()
      }
      await expect(chip).toHaveCount(0)

      // The short ones finish first: their panes close, the rest stay.
      await expect(panes).toHaveCount(4, { timeout: 20_000 })
      for (const id of short)
        await expect(workspace.locator(`[data-pane-id="${id}"]`)).toHaveCount(0)
      for (const id of long) await expect(workspace.locator(`[data-pane-id="${id}"]`)).toBeVisible()
      await expect(chip).toHaveText('+4')

      // Then the long ones.
      await expect(panes).toHaveCount(1, { timeout: 25_000 })
      await expect(chip).toHaveText('+7')
    } finally {
      setSubagentLimit(null)
    }
  })
})
