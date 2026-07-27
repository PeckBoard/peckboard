import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Chat toolbar agent status + stall detection:
 *
 *  - `mock:crash` ends the turn with `status: 'crashed'`; the toolbar pill
 *    must read "Crashed" with the crash dot, not "Idle".
 *  - A `user` event with no agent event after it used to spin the thinking
 *    dots forever. After the stall threshold the row must say the agent may
 *    have stopped and offer Retry / Terminate, and must clear again the
 *    moment any new event arrives.
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
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-status-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-status', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'agent status', folder_id: folder.id },
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

test('mock:crash leaves the toolbar status on Crashed, not Idle', async ({ request, page }) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  const status = page.getByTestId('chat-toolbar-status')
  await expect(status).toBeVisible({ timeout: 10_000 })

  const send = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'crash please', model: 'mock:crash' },
  })
  expect(send.ok(), `send failed: ${await send.text()}`).toBeTruthy()

  await expect(page.getByText('Agent crashed')).toBeVisible({ timeout: 15_000 })
  await expect(status).toHaveText('Crashed')
  await expect(status.locator('.status-dot-crashed')).toBeVisible()
})

test('a silent turn stalls after the threshold and recovers on the next event', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)

  // Fake timers so the 90s threshold doesn't cost 90s of wall clock.
  await page.clock.install()
  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.getByTestId('chat-toolbar-status')).toBeVisible({ timeout: 10_000 })

  // A bare `user` event: no dispatch, so no agent-start / agent-end ever
  // follows — exactly the "process died before emitting" shape.
  const injected = await request.post(`/api/sessions/${sessionId}/events`, {
    headers: authHeader,
    data: { kind: 'user', data: { text: 'hello?' } },
  })
  expect(injected.ok(), `inject user failed: ${await injected.text()}`).toBeTruthy()

  const stall = page.getByTestId('chat-stall')
  await expect(page.locator('.chat-thinking-dots')).toBeVisible({ timeout: 10_000 })
  await expect(stall).toBeHidden()

  // Past the threshold: the dots give way to the stall notice + actions.
  await page.clock.fastForward(100_000)
  await expect(stall).toBeVisible()
  await expect(stall).toContainText('No response — the agent may have stopped')
  await expect(page.getByTestId('chat-stall-retry')).toBeVisible()
  await expect(page.getByTestId('chat-stall-terminate')).toBeVisible()
  await expect(page.locator('.chat-thinking-dots')).toBeHidden()

  // Any new event clears it. The clock goes back to real time first: the
  // server stamps events with its own (unfaked) clock, so a page still
  // running 100s ahead would judge a brand-new event as already stale.
  await page.clock.setSystemTime(new Date())
  const late = await request.post(`/api/sessions/${sessionId}/events`, {
    headers: authHeader,
    data: { kind: 'agent-start', data: { model: 'mock:echo' } },
  })
  expect(late.ok(), `inject agent-start failed: ${await late.text()}`).toBeTruthy()

  await expect(stall).toBeHidden({ timeout: 10_000 })
  await expect(page.locator('.chat-thinking-dots')).toBeVisible()
})
