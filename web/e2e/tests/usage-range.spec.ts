import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { WebSocketImpl, type WsMessageEvent } from './ws-compat'

/**
 * E2E for the Usage Dashboard's date range, refresh, and as-of stamp.
 *
 * Seeds one `mock:usage` turn, then drives the range bar: the caption must
 * name the window the figures describe, narrowing to a window in the past
 * must refetch every panel (the totals drop to zero), switching back must
 * restore them, and Refresh must move the as-of stamp.
 *
 * Mirrors the auth + folder + session + WS pattern from
 * mock-provider.spec.ts / usage-dashboard.spec.ts.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

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

/** Load a route in the browser already authenticated, and start from the
 *  default range regardless of what a previous test left in localStorage. */
async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t as string)
    localStorage.removeItem('peckboard_usage_range')
  }, token)
  await page.goto(route)
}

test('usage dashboard range picker scopes every panel and refresh moves the as-of stamp', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-range-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-range', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'range smoke', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  const collectorPromise = collectEventsUntil(baseURL!, token, session.id, 'agent-end', 15_000)
  await new Promise((r) => setTimeout(r, 250))
  const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
    headers: authHeader,
    data: { text: 'spend some tokens', model: 'mock:usage' },
  })
  expect(sendRes.ok(), `send message failed: ${await sendRes.text()}`).toBeTruthy()
  await collectorPromise

  await loadAt(page, token, '/usage')
  await expect(page.getByTestId('usage-view')).toBeVisible()

  // ── The dashboard states the window and the timezone it is describing. ──
  const caption = page.getByTestId('usage-range-caption')
  await expect(caption).toContainText('Showing')
  await expect(caption).toContainText('local time')
  await expect(page.getByTestId('usage-updated')).toContainText('updated')
  // Default range is the last 30 days, so the just-recorded spend is in it.
  // Asserted as "cost is not zero" rather than an exact figure: the server is
  // shared across the suite, so the install-wide totals are not a fixed
  // number — what this test is about is which window they cover.
  const totals = page.getByTestId('usage-totals')
  await expect(totals).not.toContainText('$0.00')

  // ── Narrowing to a window in the past refetches every panel. ──
  await page.getByTestId('usage-range-custom').click()
  await page.getByTestId('usage-range-from').fill('2020-01-01')
  await page.getByTestId('usage-range-to').fill('2020-01-02')
  // Same-year window: the year is stated once, on the end date.
  await expect(caption).toContainText('01/01 – 01/02/2020')
  // Same install, only the window changed — and no spend falls inside it.
  await expect(totals).toContainText('$0.00')

  // ── Switching back restores the figures. ──
  await page.getByTestId('usage-range-30d').click()
  await expect(totals).not.toContainText('$0.00')
  await expect(caption).not.toContainText('2020')

  // ── Refresh moves the as-of stamp. ──
  const before = await page.getByTestId('usage-updated').textContent()
  // The stamp has second resolution; wait past a tick so the change is real.
  await page.waitForTimeout(1100)
  await page.getByTestId('usage-refresh').click()
  await expect(page.getByTestId('usage-updated')).not.toHaveText(before ?? '')
  // Figures survive the refresh — it refetched the same window.
  await expect(totals).not.toContainText('$0.00')
})
