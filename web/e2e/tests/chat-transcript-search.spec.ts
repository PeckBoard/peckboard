import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Jump-to-latest pill + in-transcript search + auto-load older:
 *
 *  - Scrolled up while a new event arrives → the "↓ New messages" pill
 *    appears; clicking it returns to the bottom and re-arms autoscroll.
 *  - Ctrl+F opens the transcript search bar (the virtualized feed hides
 *    unmounted rows from the browser's own find); a query reports a match
 *    count, auto-navigates to the first match, and Enter steps through
 *    matches by scrolling the virtualizer.
 *  - With more history than the loaded page, the bar says only loaded
 *    messages are searched and "Load full history" pages the rest in.
 *  - Scrolling near the top auto-fetches the older page (the manual
 *    button stays as fallback).
 *
 * Rows are seeded via the events-injection backdoor
 * (`POST /api/sessions/:id/events`), alternating user/agent-text so the
 * fold produces one row per event.
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
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-search-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-search', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'transcript search', folder_id: folder.id },
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

/** Alternating `alpha NNN` (user) / `beta NNN` (agent) rows, zero-padded so
 *  substring queries can be made unique. */
async function seedPairs(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  sessionId: string,
  pairs: number,
) {
  for (let i = 1; i <= pairs; i++) {
    const n = String(i).padStart(3, '0')
    await injectEvent(request, authHeader, sessionId, 'user', { text: `alpha ${n}` })
    await injectEvent(request, authHeader, sessionId, 'agent-text', { text: `beta ${n}` })
  }
}

test('jump-to-latest pill appears while scrolled up and returns to the bottom; Ctrl+F searches and navigates', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)
  await seedPairs(request, authHeader, sessionId, 30)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.getByText('beta 030')).toBeVisible({ timeout: 15_000 })

  // Scroll away from the bottom, then let a new event arrive: no yank to
  // the bottom, but the pill appears.
  await page.locator('.chat-messages').evaluate((el) => {
    el.scrollTop = 0
  })
  await injectEvent(request, authHeader, sessionId, 'agent-text', { text: 'fresh pill event' })
  const pill = page.getByTestId('chat-jump-latest')
  await expect(pill).toBeVisible({ timeout: 10_000 })
  await expect(page.getByText('fresh pill event')).not.toBeVisible()

  // Click → bottom, pill gone, autoscroll re-armed.
  await pill.click()
  await expect(page.getByText('fresh pill event')).toBeVisible()
  await expect(pill).not.toBeVisible()
  await injectEvent(request, authHeader, sessionId, 'agent-text', { text: 'auto follow event' })
  await expect(page.getByText('auto follow event')).toBeVisible({ timeout: 10_000 })
  await expect(pill).not.toBeVisible()

  // Ctrl+F opens the transcript search bar instead of the browser find.
  await page.keyboard.press('Control+f')
  const bar = page.getByTestId('chat-search-bar')
  await expect(bar).toBeVisible()
  // Everything is loaded (61 events < one page) — no scope caveat.
  await expect(page.getByTestId('chat-search-scope')).not.toBeVisible()

  // A unique match auto-navigates: beta 007 lives far above the viewport.
  await page.getByTestId('chat-search-input').fill('beta 007')
  await expect(page.getByTestId('chat-search-count')).toHaveText('1/1')
  await expect(page.getByText('beta 007')).toBeVisible()
  await expect(page.locator('.chat-vrow-match')).toContainText('beta 007')

  // A broad query counts every match and Enter steps through them.
  await page.getByTestId('chat-search-input').fill('alpha')
  await expect(page.getByTestId('chat-search-count')).toHaveText('1/30')
  await page.getByTestId('chat-search-input').press('Enter')
  await expect(page.getByTestId('chat-search-count')).toHaveText('2/30')
  await page.getByTestId('chat-search-prev').click()
  await expect(page.getByTestId('chat-search-count')).toHaveText('1/30')

  // Escape closes and clears.
  await page.getByTestId('chat-search-input').press('Escape')
  await expect(bar).not.toBeVisible()
})

test('search beyond the loaded page: scope caveat + Load full history', async ({
  request,
  page,
}) => {
  test.slow() // seeds 220 events over HTTP
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)
  await seedPairs(request, authHeader, sessionId, 110)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.getByText('beta 110')).toBeVisible({ timeout: 15_000 })

  // The initial page holds the newest 200 events, so `alpha 007` (event
  // 13 of 220) is NOT loaded: the bar must say so instead of silently
  // reporting no matches anywhere.
  await page.getByTestId('chat-search-toggle').click()
  await page.getByTestId('chat-search-input').fill('alpha 007')
  await expect(page.getByTestId('chat-search-count')).toHaveText('No matches')
  await expect(page.getByTestId('chat-search-scope')).toContainText('loaded messages only')

  // Load the rest → the caveat retires and the old row becomes a match.
  await page.getByTestId('chat-search-load-all').click()
  await expect(page.getByTestId('chat-search-scope')).not.toBeVisible({ timeout: 15_000 })
  await expect(page.getByTestId('chat-search-count')).toHaveText('1/1')
  await page.getByTestId('chat-search-input').press('Enter')
  await expect(page.getByText('alpha 007')).toBeVisible()
})

test('scrolling near the top auto-loads the older page', async ({ request, page }) => {
  test.slow() // seeds 220 events over HTTP
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)
  await seedPairs(request, authHeader, sessionId, 110)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.getByText('beta 110')).toBeVisible({ timeout: 15_000 })
  await expect(page.getByTestId('chat-load-older')).toBeVisible()

  // Nearing the top fetches the remaining 20 events without a click; the
  // short page proves history is exhausted and retires the button.
  await expect
    .poll(
      async () => {
        await page.locator('.chat-messages').evaluate((el) => {
          el.scrollTop = 0
        })
        return page.getByTestId('chat-load-older').isVisible()
      },
      { timeout: 15_000 },
    )
    .toBe(false)

  // The prepended rows are reachable at the top.
  await page.locator('.chat-messages').evaluate((el) => {
    el.scrollTop = 0
  })
  await expect(page.getByText('alpha 001')).toBeVisible({ timeout: 10_000 })
})
