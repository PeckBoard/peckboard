import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { WebSocketImpl, type WsMessageEvent } from './ws-compat'

/**
 * E2E for the two figures the usage dashboard used to get wrong.
 *
 * 1. The per-session context gauge is sized from the SESSION'S model. A
 *    session on a 1M-context model measured against the shared 200K default
 *    rendered a full, red gauge reading 100% that was simply false; a session
 *    whose model we can't resolve must label its denominator as the default
 *    rather than presenting it as that model's real limit.
 * 2. The header "Billed Tokens" card and the session rows must be the same
 *    field. They used to be `total_tokens` and `total_tokens_used`
 *    respectively, so the panel rows did not add up to the card above them.
 *
 * Both sessions run the deterministic `mock:usage` scenario for their usage;
 * the 1M model is set with a PATCH afterwards, so the assertion needs no real
 * provider call. Auth + WS helpers mirror mock-provider.spec.ts.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

/** A model id the registry advertises with a 1M context window. */
const LONG_CONTEXT_MODEL = 'claude:opus[1m]'

type AuthBundle = { token: string; authHeader: { Authorization: string } }

async function authenticate(request: APIRequestContext): Promise<AuthBundle> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

type WsEvent = { kind: string; data: Record<string, unknown>; seq: number }

/** Open a WS connection, authenticate, subscribe, and collect every event for
 *  `sessionId` until `untilKind` is observed. */
async function collectEventsUntil(
  baseURL: string,
  token: string,
  sessionId: string,
  untilKind: string,
  timeoutMs: number,
): Promise<WsEvent[]> {
  const wsUrl = baseURL.replace(/^http/, 'ws') + '/ws'
  const ws = new WebSocketImpl(wsUrl)
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
      const handler = (msg: WsMessageEvent) => {
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

/** Load a route already authenticated, by seeding the token the SPA reads
 *  from localStorage before any script runs. */
async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t as string)
  }, token)
  await page.goto(route)
}

/** The frontend's compact token formatter (`util/format.ts`), duplicated here
 *  so the test can predict the exact rendered string from the API's numbers
 *  instead of hardcoding a figure that other specs' sessions would break. */
function fmtTokens(n: number): string {
  const abs = Math.abs(n)
  if (abs >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(2)}B`
  if (abs >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`
  if (abs >= 1_000) return `${(n / 1_000).toFixed(1)}K`
  return `${Math.round(n)}`
}

type SessionUsageRow = {
  id: string
  name: string
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_creation_tokens: number
  total_context_tokens: number
  model: string | null
}

/** Run the `mock:usage` scenario once in `sessionId`, waiting for the usage
 *  row to be persisted (it lands before `agent-end`). */
async function runMockUsage(
  request: APIRequestContext,
  baseURL: string,
  token: string,
  authHeader: { Authorization: string },
  sessionId: string,
) {
  const collector = collectEventsUntil(baseURL, token, sessionId, 'agent-end', 15_000)
  await new Promise((r) => setTimeout(r, 250))
  const sendRes = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'measure my context', model: 'mock:usage' },
  })
  expect(sendRes.ok(), `send message failed: ${await sendRes.text()}`).toBeTruthy()
  const events = await collector
  expect(
    events.some((e) => e.kind === 'agent-usage'),
    'agent-usage event was emitted',
  ).toBeTruthy()
}

test('session context gauges are sized per model and the header reconciles with the rows', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-gauge-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-gauge', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const names = { wide: 'gauge wide model', plain: 'gauge unknown model' }
  const ids: Record<string, string> = {}
  for (const [key, name] of Object.entries(names)) {
    const res = await request.post('/api/sessions', {
      headers: authHeader,
      data: { name, folder_id: folder.id },
    })
    expect(res.ok(), `create session failed: ${await res.text()}`).toBeTruthy()
    ids[key] = ((await res.json()) as { id: string }).id
  }

  // Same recorded usage in both, so the only difference between the two rows
  // is the model their gauge is measured against.
  await runMockUsage(request, baseURL!, token, authHeader, ids.wide)
  await runMockUsage(request, baseURL!, token, authHeader, ids.plain)

  // Set the long-context model AFTER the run: dispatching a message pins the
  // session to the model it ran with, which would overwrite this.
  const patchRes = await request.patch(`/api/sessions/${ids.wide}`, {
    headers: authHeader,
    data: { model: LONG_CONTEXT_MODEL },
  })
  expect(patchRes.ok(), `set model failed: ${await patchRes.text()}`).toBeTruthy()

  // The API is the source of truth for what the UI should render, so read it
  // and derive every expected string from it.
  const apiRes = await request.get('/api/usage/sessions', { headers: authHeader })
  expect(apiRes.ok(), `usage sessions failed: ${await apiRes.text()}`).toBeTruthy()
  const rows = (await apiRes.json()) as SessionUsageRow[]
  const billed = (r: SessionUsageRow) =>
    r.input_tokens + r.output_tokens + r.cache_read_tokens + r.cache_creation_tokens
  const wideRow = rows.find((r) => r.id === ids.wide)!
  const plainRow = rows.find((r) => r.id === ids.plain)!
  expect(wideRow.model, 'the 1M model reached the rollup').toBe(LONG_CONTEXT_MODEL)
  expect(billed(wideRow), 'the mock scenario recorded billed tokens').toBeGreaterThan(0)
  expect(wideRow.total_context_tokens, 'and a context snapshot').toBeGreaterThan(0)

  await loadAt(page, token, '/usage')
  await expect(page.getByTestId('usage-view')).toBeVisible()

  // ── Gauge denominator: known 1M-context model. ──
  const wide = page.getByTestId('usage-session-row').filter({ hasText: names.wide })
  await expect(wide).toBeVisible()
  // Measured against 1M, named, and NOT flagged as a default.
  await expect(wide).toContainText('/ 1.00M')
  await expect(wide).toContainText('opus[1m]')
  await expect(wide).not.toContainText('default')
  // 1.5K of 1M is 0%, so the gauge is nowhere near the danger band the 200K
  // default used to put it in.
  await expect(wide).toContainText('(0%)')
  await expect(wide.locator('.usage-gauge-fill.is-danger')).toHaveCount(0)

  // ── Gauge denominator: model we can't resolve. ──
  const plain = page.getByTestId('usage-session-row').filter({ hasText: names.plain })
  await expect(plain).toBeVisible()
  await expect(plain).toContainText(`/ ${fmtTokens(200_000)} default`)

  // ── Header card vs panel rows: one field per label. ──
  // Every row shows its billed-token total…
  await expect(wide).toContainText(fmtTokens(billed(wideRow)))
  await expect(plain).toContainText(fmtTokens(billed(plainRow)))
  // …and the header card is the sum of exactly that figure over every session,
  // so the two reconcile.
  const totalBilled = rows.reduce((s, r) => s + billed(r), 0)
  await expect(page.getByTestId('usage-stat-billed-tokens-value')).toHaveText(
    fmtTokens(totalBilled),
  )
  await expect(page.getByTestId('usage-stat-billed-tokens')).toContainText('Billed Tokens')

  // ── Context card is the largest single session, not a meaningless sum. ──
  const largest = Math.max(...rows.map((r) => r.total_context_tokens))
  const summed = rows.reduce((s, r) => s + r.total_context_tokens, 0)
  expect(summed, 'two sessions carry context, so the sum differs from the max').toBeGreaterThan(
    largest,
  )
  await expect(page.getByTestId('usage-stat-largest-context')).toContainText('Largest Context')
  await expect(page.getByTestId('usage-stat-largest-context-value')).toHaveText(fmtTokens(largest))

  // ── Detail page agrees with the row it was opened from. ──
  // The turn ran on `mock:usage`, which resolves to no known window, so the
  // gauge falls back to the session's configured model instead of showing a
  // 200K denominator the list contradicts.
  await wide.click()
  const detailGauge = page.getByTestId('usage-detail-context')
  await expect(detailGauge).toBeVisible()
  await expect(detailGauge).toContainText('/ 1.00M')
  await expect(detailGauge).toContainText('opus[1m]')
  await expect(page.getByTestId('usage-detail-totals')).toContainText('Billed Tokens')
  await page.getByRole('button', { name: '← Usage' }).click()
  await expect(page.getByTestId('usage-totals')).toBeVisible()

  // ── Costs read as estimates in USD, with the rate table named. ──
  await expect(page.getByTestId('usage-stat-cost')).toContainText('Est. cost (USD)')
  await expect(page.getByTestId('usage-cost-footnote')).toContainText('/api/usage/costs')
  await expect(page.getByTestId('usage-cost-footnote')).toContainText('estimates')
})
