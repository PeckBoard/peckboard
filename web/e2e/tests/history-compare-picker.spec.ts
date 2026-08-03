import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { WebSocketImpl, type WsMessageEvent } from './ws-compat'

/**
 * Regression for the history-compare picker: a third click must drop the
 * least-recently-clicked version, not the lowest-numbered one. Reproduces
 * the exact sequence from the bug report: default selection [v2, v3],
 * click v1, then click v2 — the diff shown must be v1 → v2, not v2 → v3.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const DOC = ['# Onboarding', '', 'The team ships on Fridays.', ''].join('\n')

type Auth = { token: string; auth: Record<string, string> }

async function authenticate(request: APIRequestContext): Promise<Auth> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

type WsEvent = { kind: string; data: Record<string, unknown>; seq: number }

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
              `Did not see '${untilKind}' within ${timeoutMs}ms; got: ${collected
                .map((e) => e.kind)
                .join(', ')}`,
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

async function seedFolder(
  request: APIRequestContext,
  auth: Record<string, string>,
  suffix: string,
): Promise<{ id: string; dir: string }> {
  const dir = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-history-${suffix}-`))
  mkdirSync(path.join(dir, 'docs'), { recursive: true })
  writeFileSync(path.join(dir, 'docs', 'onboarding.md'), DOC)
  const name = `e2e-history-${suffix}-${Date.now()}`
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name, path: dir },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  const folder = (await res.json()) as { id: string }
  return { id: folder.id, dir }
}

async function createReview(
  request: APIRequestContext,
  auth: Record<string, string>,
  data: Record<string, unknown>,
): Promise<string> {
  const res = await request.post('/api/doc-reviews', { headers: auth, data })
  expect(res.ok(), `create review failed: ${await res.text()}`).toBeTruthy()
  const body = (await res.json()) as { review: { id: string } }
  return body.review.id
}

async function armReviewer(
  request: APIRequestContext,
  auth: Record<string, string>,
  reviewId: string,
): Promise<string> {
  const res = await request.post(`/api/doc-reviews/${reviewId}/pass`, {
    headers: auth,
    data: {
      message: '[mock:chat] warming up the reviewer',
      include_annotations: false,
      model: 'mock:doc-review',
    },
  })
  expect(res.ok(), `arm pass failed: ${await res.text()}`).toBeTruthy()
  const { session_id: sessionId } = (await res.json()) as { session_id: string }

  await expect
    .poll(
      async () => {
        const events = await request.get(`/api/sessions/${sessionId}/events?limit=1000`, {
          headers: auth,
        })
        if (!events.ok()) return 0
        return ((await events.json()) as WsEvent[]).filter((e) => e.kind === 'agent-end').length
      },
      { timeout: 20_000, message: 'the warm-up turn never finished' },
    )
    .toBeGreaterThan(0)

  return sessionId
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto(route)
}

async function annotate(page: Page, line: number, action: string, body: string) {
  await page.locator(`[data-testid="review-block"][data-line-start="${line}"]`).first().click()
  await expect(page.getByTestId('review-popover')).toBeVisible()
  await page.getByTestId(`review-popover-${action}`).click()
  await page.getByTestId('review-annotation-editor').fill(body)
  await page.getByTestId('review-annotation-submit').click()
  await expect(page.getByTestId('review-popover')).toHaveCount(0)
}

async function cleanupReviews(request: APIRequestContext, auth: Record<string, string>) {
  const res = await request.get('/api/doc-reviews', { headers: auth })
  if (!res.ok()) return
  for (const r of ((await res.json()) as { reviews: Array<{ id: string }> }).reviews) {
    await request.delete(`/api/doc-reviews/${r.id}`, { headers: auth })
  }
}

test.describe('history tab compare picker', () => {
  test.afterEach(async ({ request }) => {
    const { auth } = await authenticate(request)
    await cleanupReviews(request, auth)
  })

  test('a third click drops the least-recently-clicked version, not the lowest-numbered one', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL, 'baseURL configured').toBeTruthy()
    const { token, auth } = await authenticate(request)
    const folder = await seedFolder(request, auth, 'compare')
    const reviewId = await createReview(request, auth, {
      source_kind: 'file',
      source_ref: `${folder.id}:docs/onboarding.md`,
      title: 'Compare picker review',
    })
    const sessionId = await armReviewer(request, auth, reviewId)

    await loadAt(page, token, `/review/${reviewId}`)
    await expect(page.getByTestId('review-view')).toBeVisible({ timeout: 15_000 })

    // v1 -> v2 via a pass.
    await annotate(page, 3, 'wrong', 'we ship on Tuesdays')
    const events = collectEventsUntil(baseURL!, token, sessionId, 'agent-end', 30_000)
    await page.getByTestId('review-run-pass').click()
    await events
    await expect(page.getByTestId('review-version')).toHaveText('v2', { timeout: 15_000 })

    // v2 -> v3 via a revert to v1.
    await page.getByTestId('review-tab-history').click()
    await page
      .locator('[data-testid="review-version-item"][data-version="1"]')
      .getByTestId('review-revert')
      .click()
    await page.getByTestId('confirm-dialog-confirm').click()
    await expect(page.getByTestId('review-version')).toHaveText('v3', { timeout: 15_000 })

    // Default selection is [v2, v3].
    const diffTitle = page.locator('.review-rail__group-title').first()
    await expect(diffTitle).toContainText('v2 → v3')

    // Click v1, then click v2 — the two most recently clicked rows.
    await page
      .locator('[data-testid="review-version-item"][data-version="1"]')
      .locator('.review-version-row__main')
      .click()
    await page
      .locator('[data-testid="review-version-item"][data-version="2"]')
      .locator('.review-version-row__main')
      .click()

    await expect(diffTitle).toContainText('v1 → v2')
    await expect(diffTitle).toHaveText('v1 → v2')
  })
})
