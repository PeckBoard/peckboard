import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E for the Usage Dashboard.
 *
 * Drives the deterministic `mock:usage` provider scenario, which emits an
 * Edit tool call, an ask_expert consultation, and a per-turn Usage event —
 * the source of every figure the dashboard reads. The seeded session is
 * linked to a project so both the per-session and per-project rollups
 * populate. Then it opens the Usage view and asserts the entity rollups, a
 * file-update operation-cost breakdown, and a token trend series all render
 * with non-zero values.
 *
 * Mirrors the auth + folder + session + WS pattern from
 * mock-provider.spec.ts; the localStorage-token UI login is copied from
 * expert-view.spec.ts.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type AuthBundle = { token: string; authHeader: { Authorization: string } }

async function authenticate(request: APIRequestContext): Promise<AuthBundle> {
  // The server auto-bootstraps the admin from PECKBOARD_BOOTSTRAP_* env
  // vars at first start (see playwright.config.ts); we just log in.
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

type WsEvent = { kind: string; data: Record<string, unknown>; seq: number }

/**
 * Open a WS connection, authenticate, subscribe, and collect every event
 * for `sessionId` until `untilKind` is observed (typically `agent-end`).
 */
async function collectEventsUntil(
  baseURL: string,
  token: string,
  sessionId: string,
  untilKind: string,
  timeoutMs: number,
): Promise<WsEvent[]> {
  const wsUrl = baseURL.replace(/^http/, 'ws') + '/ws'
  const ws = new WebSocket(wsUrl)
  const collected: WsEvent[] = []

  try {
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error(`WS handshake timed out after ${timeoutMs}ms`)),
        timeoutMs,
      )
      ws.addEventListener('open', () => {
        clearTimeout(timer)
        resolve()
      })
      ws.addEventListener('error', (err) => {
        clearTimeout(timer)
        reject(new Error(`WS error: ${String(err)}`))
      })
    })

    ws.send(JSON.stringify({ type: 'auth', token }))
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('WS auth_ok not received')), timeoutMs)
      const handler = (msg: MessageEvent) => {
        const frame = JSON.parse(String(msg.data))
        if (frame.type === 'auth_ok') {
          clearTimeout(timer)
          ws.removeEventListener('message', handler)
          resolve()
        }
      }
      ws.addEventListener('message', handler)
    })

    ws.send(JSON.stringify({ type: 'subscribe', session_id: sessionId }))

    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(
        () =>
          reject(
            new Error(
              `Did not see '${untilKind}' within ${timeoutMs}ms; got: ${collected.map((e) => e.kind).join(', ')}`,
            ),
          ),
        timeoutMs,
      )
      ws.addEventListener('message', (msg) => {
        const frame = JSON.parse(String(msg.data))
        if (frame.type !== 'event' || frame.session_id !== sessionId) return
        const ev = frame.event as WsEvent
        collected.push(ev)
        if (ev.kind === untilKind) {
          clearTimeout(timer)
          resolve()
        }
      })
    })
  } finally {
    ws.close()
  }

  return collected
}

/** Load a route in the browser already authenticated, by seeding the token
 *  the SPA reads from localStorage before any script runs. */
async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t as string)
  }, token)
  await page.goto(route)
}

test('usage dashboard reflects mock:usage activity with non-zero rollups, costs, and trends', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)

  // ── Seed: folder + project + session, linked so both the per-session
  //    and per-project rollups populate from the same usage. ──
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-usage-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-usage', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: authHeader,
    // worker_count 0 so creating the project doesn't auto-spawn workers;
    // we drive the one usage session ourselves.
    data: { name: 'Usage Demo', folder_id: folder.id, workflow: 'task', worker_count: 0 },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string; name: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'usage smoke', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  const patchRes = await request.patch(`/api/sessions/${session.id}`, {
    headers: authHeader,
    data: { project_id: project.id },
  })
  expect(patchRes.ok(), `link session to project failed: ${await patchRes.text()}`).toBeTruthy()

  // Subscribe BEFORE sending so we can't miss agent-start. The Usage event
  // (and its usage_events row) is emitted before Completed, so by the time
  // agent-end arrives the dashboard data is persisted.
  const collectorPromise = collectEventsUntil(baseURL!, token, session.id, 'agent-end', 15_000)
  await new Promise((r) => setTimeout(r, 250))

  const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
    headers: authHeader,
    data: { text: 'go', model: 'mock:usage' },
  })
  expect(sendRes.ok(), `send message failed: ${await sendRes.text()}`).toBeTruthy()

  const events = await collectorPromise
  // Backend sanity: the scenario emitted the usage + operation events the
  // dashboard derives from.
  expect(
    events.some((e) => e.kind === 'agent-usage'),
    'agent-usage event was emitted',
  ).toBeTruthy()
  expect(
    events.some((e) => e.kind === 'agent-tool-start' && e.data.name === 'Edit'),
    'an Edit tool call was emitted',
  ).toBeTruthy()

  // ── UI: open the Usage view and assert it shows real numbers. ──
  await loadAt(page, token, '/usage')
  await expect(page.getByTestId('usage-view')).toBeVisible()

  // Entity rollup #1 — per session. fmtTokens(2600) === "2.6K".
  const sessionRow = page.getByTestId('usage-session-row').filter({ hasText: 'usage smoke' })
  await expect(sessionRow).toBeVisible()
  await expect(sessionRow).toContainText('2.6K')

  // Entity rollup #2 — per project (the linked session's spend).
  const projectRow = page.getByTestId('usage-project-row').filter({ hasText: project.name })
  await expect(projectRow).toBeVisible()
  await expect(projectRow).toContainText('2.6K')

  // Operation-cost breakdown — File Updates. The edited path is listed and
  // the subtotal is a real, non-zero dollar figure.
  const fileUpdates = page.getByTestId('usage-cost-file_update')
  await expect(fileUpdates).toContainText('lib.rs')
  await expect(page.getByTestId('usage-cost-file_update-subtotal')).not.toHaveText('$0.00')

  // Trend series — the overall tokens chart renders with a labelled series.
  await expect(page.getByTestId('usage-trend-tokens-chart')).toBeVisible()
  await expect(page.getByTestId('usage-trend-tokens-legend')).toContainText('Overall')
})
