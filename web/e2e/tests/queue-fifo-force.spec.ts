import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Durable message queue: FIFO + per-message force ("Send now").
 *
 *  - Messages sent while the agent is busy NEVER interrupt it: they park
 *    in the per-session FIFO. TWO queued messages must both survive (the
 *    old single-slot queue silently overwrote the first) and drain in
 *    send order, one agent turn each, once the busy run ends.
 *  - Each queued message renders as a chip with a "Send now" button that
 *    forces it through immediately — for a per-turn provider that means
 *    the busy run is cancelled and the forced message dispatched next,
 *    ahead of the rest of the queue.
 *  - The chip's ✕ removes a queued message without it ever being sent.
 *
 * All driven on the mock provider: `mock:block` parks mid-tool until
 * interrupted, `mock:echo` echoes its input as agent-text.
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
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-queue-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-queue', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'queue fifo', folder_id: folder.id },
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

async function send(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  sessionId: string,
  text: string,
  model: string,
) {
  const res = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text, model },
  })
  expect(res.ok(), `send failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { status: string }).status
}

async function queuedTexts(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  sessionId: string,
) {
  const res = await request.get(`/api/sessions/${sessionId}/queue`, { headers: authHeader })
  expect(res.ok()).toBeTruthy()
  const body = (await res.json()) as { messages: { id: number; text: string }[] }
  return body.messages.map((m) => m.text)
}

/** Echoed agent-text values, in event order, via the REST event log. */
async function echoedTexts(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  sessionId: string,
) {
  const res = await request.get(`/api/sessions/${sessionId}/events`, { headers: authHeader })
  expect(res.ok()).toBeTruthy()
  const events = (await res.json()) as { kind: string; data: { text?: string } }[]
  return events
    .filter((e) => e.kind === 'agent-text')
    .map((e) => String(e.data.text ?? ''))
    .filter((t) => t.startsWith('drain-'))
}

test('two messages queued while busy both survive and drain in FIFO order', async ({ request }) => {
  const { authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)

  expect(await send(request, authHeader, sessionId, 'park here', 'mock:block')).toBe('started')

  // Both mid-turn sends queue — neither interrupts the blocked run.
  expect(await send(request, authHeader, sessionId, 'drain-first', 'mock:echo')).toBe('queued')
  expect(await send(request, authHeader, sessionId, 'drain-second', 'mock:echo')).toBe('queued')

  // The FIFO holds BOTH, oldest first (regression: the old single-slot
  // queue overwrote 'drain-first').
  expect(await queuedTexts(request, authHeader, sessionId)).toEqual(['drain-first', 'drain-second'])

  // Release the blocked run; the completion listener drains one message
  // per agent turn, in order.
  const interruptRes = await request.post(`/api/sessions/${sessionId}/interrupt`, {
    headers: authHeader,
  })
  expect(interruptRes.status()).toBe(204)

  await expect
    .poll(() => echoedTexts(request, authHeader, sessionId), { timeout: 15_000 })
    .toEqual(['drain-first', 'drain-second'])
  expect(await queuedTexts(request, authHeader, sessionId)).toEqual([])
})

test('"Send now" forces a queued message through, interrupting the busy agent', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-vrow').first())).toBeVisible({
    timeout: 10_000,
  })

  expect(await send(request, authHeader, sessionId, 'park here', 'mock:block')).toBe('started')
  await expect(page.getByText('working…')).toBeVisible({ timeout: 10_000 })

  expect(await send(request, authHeader, sessionId, 'drain-forced', 'mock:echo')).toBe('queued')
  expect(await send(request, authHeader, sessionId, 'drain-patient', 'mock:echo')).toBe('queued')

  // One chip per queued message, each with its Send now button.
  await expect(page.getByTestId('chat-queued-chip')).toHaveCount(2, { timeout: 10_000 })

  // Force the FIRST queued message through. The blocked run is
  // cancelled, the forced message dispatches immediately, and the
  // remaining queue drains behind it.
  await page
    .getByTestId('chat-queued-chip')
    .filter({ hasText: 'drain-forced' })
    .getByTestId('chat-queued-force')
    .click()

  await expect
    .poll(() => echoedTexts(request, authHeader, sessionId), { timeout: 15_000 })
    .toEqual(['drain-forced', 'drain-patient'])
  await expect(page.getByTestId('chat-queued-chip')).toHaveCount(0, { timeout: 10_000 })
})

test('the chip ✕ removes a queued message without sending it', async ({ request, page }) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-vrow').first())).toBeVisible({
    timeout: 10_000,
  })

  expect(await send(request, authHeader, sessionId, 'park here', 'mock:block')).toBe('started')
  await expect(page.getByText('working…')).toBeVisible({ timeout: 10_000 })

  expect(await send(request, authHeader, sessionId, 'drain-doomed', 'mock:echo')).toBe('queued')
  await expect(page.getByTestId('chat-queued-chip')).toHaveCount(1, { timeout: 10_000 })

  await page.getByTestId('chat-queued-remove').click()
  await expect(page.getByTestId('chat-queued-chip')).toHaveCount(0, { timeout: 10_000 })
  // The chip removal is optimistic — confirm the DELETE actually landed
  // server-side before releasing the run.
  await expect
    .poll(() => queuedTexts(request, authHeader, sessionId), { timeout: 5_000 })
    .toEqual([])

  // The removed message must never echo, even after the run ends.
  const interruptRes = await request.post(`/api/sessions/${sessionId}/interrupt`, {
    headers: authHeader,
  })
  expect(interruptRes.status()).toBe(204)
  await expect(
    page.locator('.chat-agent-start-label').filter({ hasText: 'Agent interrupted' }),
  ).toBeVisible({ timeout: 10_000 })
  await page.waitForTimeout(1_000)
  expect(await echoedTexts(request, authHeader, sessionId)).toEqual([])
})
