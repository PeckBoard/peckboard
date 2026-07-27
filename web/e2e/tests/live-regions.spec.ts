import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Live regions (screen-reader announcements).
 *
 * A screen reader only announces a MUTATION of a live region it was already
 * observing. Both of the app's live regions used to enter the DOM together
 * with their text — the connection banner rendered `null` until a 1500ms
 * timer fired, and the conversation log rode on the virtualized scroller —
 * so the announcement was commonly dropped (or, on the scroller, replayed
 * for old rows as the virtualizer remounted them while scrolling).
 *
 * Both tests below tag the empty container with a `data-e2e-marked`
 * attribute React does not manage, then assert the text later appears
 * inside that SAME node. That is the property that matters: container
 * first, text second.
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
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-live-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-live', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'live regions', folder_id: folder.id },
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

/** Stamp an attribute React doesn't own, to prove node identity later. */
async function markNode(page: Page, testId: string) {
  await page.getByTestId(testId).evaluate((el) => {
    el.setAttribute('data-e2e-marked', '1')
  })
  return page.locator(`[data-testid="${testId}"][data-e2e-marked="1"]`)
}

test('chat live region announces turn boundaries, and the scroller is not a log', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.getByTestId('chat-toolbar-status')).toBeVisible({ timeout: 10_000 })

  // The region exists — and is empty — before anything is announced.
  const region = page.getByTestId('chat-live-region')
  await expect(region).toBeAttached()
  await expect(region).toHaveText('')
  await expect(region).toHaveAttribute('aria-live', 'polite')
  await expect(region).toHaveAttribute('role', 'status')
  const marked = await markNode(page, 'chat-live-region')

  // The virtualized scroller must NOT be a live region: rows mount and
  // unmount as the user scrolls, so `role="log"` (which carries an implicit
  // `aria-live="polite"`) replayed old messages on scroll.
  const scroller = await page.locator('.chat-messages').evaluate((el) => ({
    role: el.getAttribute('role'),
    live: el.getAttribute('aria-live'),
  }))
  expect(scroller.live).toBeNull()
  expect(scroller.role).not.toBe('log')

  // `mock:echo` replies with the message text, so the turn boundary is
  // deterministic. The announcement is the turn boundary only — it must NOT
  // echo the reply body, or every word of every message would exist twice in
  // the accessibility tree.
  const send = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'live region check', model: 'mock:echo' },
  })
  expect(send.ok(), `send failed: ${await send.text()}`).toBeTruthy()

  // Announced into the node that was already there, not a fresh one.
  // `toHaveText` is a full-string match: it also proves the reply body is
  // not echoed into the region.
  await expect(marked).toHaveText('Agent replied', { timeout: 15_000 })
})

test('connection status container exists before its text', async ({ request, page, baseURL }) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token } = await authenticate(request)

  // Fake timers: the banner's 1500ms grace period becomes deterministic, so
  // "the container is empty first" is a real assertion, not a race.
  await page.clock.install()

  // The page-side socket opens but the server half is ours — withholding
  // `auth_ok` keeps the app disconnected, like a server that stopped
  // responding.
  let serverHalf: { send: (data: string) => void } | null = null
  await page.routeWebSocket(/\/ws$/, (ws) => {
    serverHalf = ws
  })

  await loadAppAt(page, token, '/')

  const region = page.getByTestId('connection-status')
  await expect(region).toBeAttached({ timeout: 10_000 })
  await expect(region).toHaveAttribute('aria-live', 'polite')
  await expect(region).toHaveAttribute('role', 'status')
  // Container first: mounted and empty while the grace period runs.
  await expect(region).toHaveText('')
  await expect(page.getByTestId('connection-banner')).toHaveCount(0)
  const marked = await markNode(page, 'connection-status')

  // Text second — into the same node.
  await page.clock.fastForward(2000)
  await expect(marked).toContainText('Connection lost')
  await expect(page.getByTestId('connection-banner')).toBeVisible()

  // Reconnect: the banner goes, the region stays, and the recovery is
  // announced so a screen-reader user learns their input will send again.
  expect(serverHalf, 'WS route captured').toBeTruthy()
  serverHalf!.send(JSON.stringify({ type: 'auth_ok', user_id: 'e2e' }))
  await expect(page.getByTestId('connection-banner')).toHaveCount(0, { timeout: 10_000 })
  await page.clock.runFor(50)
  await expect(marked).toHaveText('Connection restored')
  await expect(marked).toBeAttached()
})
