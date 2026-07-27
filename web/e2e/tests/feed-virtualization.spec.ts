import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Feed performance + resilience surfaces (virtualized feed card):
 *
 *  - A session with hundreds of distinct rows keeps the DOM bounded: the
 *    virtualizer renders only the viewport's `.chat-vrow` nodes, not one
 *    per display item.
 *  - "Load older messages" prepends the previous page without losing the
 *    button, and stick-to-bottom keeps the newest row in view when a new
 *    event arrives over WS.
 *  - Sending while a turn is running shows the queued chip.
 *  - An unrecognized event kind renders the fallback row with its kind
 *    label and raw JSON instead of vanishing.
 *
 * Rows are seeded via the events-injection backdoor
 * (`POST /api/sessions/:id/events`), alternating user/agent-text so the
 * fold produces one row per event (consecutive agent-text would merge).
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

async function seedSession(request: APIRequestContext, authHeader: Record<string, string>) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-virt-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-virt', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'virtual feed', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  return session.id
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

async function injectEvent(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  sessionId: string,
  kind: string,
  data: Record<string, unknown>,
) {
  const res = await request.post(`/api/sessions/${sessionId}/events`, {
    headers: authHeader,
    data: { kind, data },
  })
  expect(res.ok(), `inject ${kind} failed: ${await res.text()}`).toBeTruthy()
}

test('500 alternating events: bounded DOM rows, Load older prepends, stick-to-bottom follows new events', async ({
  request,
  page,
}) => {
  test.slow() // seeding 500 events over HTTP takes a while
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)

  // 250 user + 250 agent-text rows, interleaved so nothing folds.
  for (let i = 1; i <= 250; i++) {
    await injectEvent(request, authHeader, sessionId, 'user', { text: `question ${i}` })
    await injectEvent(request, authHeader, sessionId, 'agent-text', { text: `reply ${i}` })
  }

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  // Newest row visible (stick-to-bottom on initial load)...
  await expect(page.getByText('reply 250')).toBeVisible({ timeout: 15_000 })
  // ...and the initial page holds EVENTS_PAGE_SIZE (200) events → ~200
  // display rows, but the virtualizer mounts only the viewport's worth.
  const rowCount = await page.locator('.chat-vrow').count()
  expect(rowCount, `virtualized row count (${rowCount}) should be far below 200`).toBeLessThan(100)
  expect(rowCount).toBeGreaterThan(3)

  // Load older: the previous page prepends (an older reply becomes
  // reachable) — the newest rows stay in the item set.
  await expect(page.getByTestId('chat-load-older')).toBeVisible()
  await page.getByTestId('chat-load-older').click()
  await expect
    .poll(
      async () => {
        // Scroll to the top of the feed to bring prepended rows into the
        // Scroll to the top of the feed to bring prepended rows into the
        // virtualizer's window. The exact page boundary depends on the
        // server's inclusive/exclusive cursor, so probe for question 52 —
        // safely inside the prepended page either way (the pre-load-older
        // top row was question ~151).
        await page.locator('.chat-messages').evaluate((el) => {
          el.scrollTop = 0
        })
        return page.getByText('question 52').isVisible()
      },
      { timeout: 15_000 },
    )
    .toBe(true)

  // Stick-to-bottom: jump back to the bottom, then a new WS event must
  // scroll into view on its own.
  await page.locator('.chat-messages').evaluate((el) => {
    el.scrollTop = el.scrollHeight
  })
  await injectEvent(request, authHeader, sessionId, 'agent-text', { text: 'fresh tail event' })
  await expect(page.getByText('fresh tail event')).toBeVisible({ timeout: 10_000 })
})

test('queued chip shows when sending during a running turn', async ({ request, page }) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-vrow').first())).toBeVisible({
    timeout: 10_000,
  })

  const first = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'stay busy', model: 'mock:block' },
  })
  expect(first.ok()).toBeTruthy()
  await expect(page.getByText('working…')).toBeVisible({ timeout: 10_000 })

  try {
    const second = await request.post(`/api/sessions/${sessionId}/message`, {
      headers: authHeader,
      data: { text: 'follow-up while busy', model: 'mock:echo' },
    })
    expect(second.ok()).toBeTruthy()
    expect(((await second.json()) as { status: string }).status).toBe('queued')

    await expect(page.getByTestId('chat-queued-chip')).toBeVisible({ timeout: 10_000 })
    await expect(page.getByTestId('chat-queued-chip')).toContainText('Queued')
  } finally {
    await request.post(`/api/sessions/${sessionId}/interrupt`, { headers: authHeader })
  }
})

test('unknown event kind renders the fallback row with kind label and raw JSON', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-vrow').first())).toBeVisible({
    timeout: 10_000,
  })

  await injectEvent(request, authHeader, sessionId, 'totally-novel-kind', { foo: 'bar', n: 7 })

  const fallback = page.getByTestId('chat-unknown-event')
  await expect(fallback).toBeVisible({ timeout: 10_000 })
  await expect(fallback).toContainText('totally-novel-kind')
  await expect(fallback).toContainText('unrecognized event')
  await fallback.locator('summary').click()
  await expect(fallback).toContainText('"foo": "bar"')
})
