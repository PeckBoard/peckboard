import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Recovery from a conversation that outgrew the context window.
 *
 * A long session eventually fails every turn with "Prompt is too long":
 * each retry resumes the same oversized conversation and fails the same
 * way. The session now heals itself: it drops the conversation, carries
 * the transcript over into a fresh one on the same model, and replays the
 * failed turn — same session, no user action.
 *
 * `mock:context-overflow` fails whenever it is handed a conversation to
 * resume (with Claude's bare "Prompt is too long" result, which the host
 * must classify itself) and, on a fresh turn, says whether the overflow
 * transcript reached it.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const NOTICE = 'Context limit reached'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function seedSession(request: APIRequestContext, auth: Record<string, string>) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-overflow-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-overflow-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'overflow recovery', folder_id: folder.id },
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

async function events(request: APIRequestContext, auth: Record<string, string>, sessionId: string) {
  const res = await request.get(`/api/sessions/${sessionId}/events`, { headers: auth })
  expect(res.ok()).toBeTruthy()
  return (await res.json()) as { kind: string; data: Record<string, unknown> }[]
}

test('"Prompt is too long" restarts the conversation fresh with the transcript and replays the turn', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const sessionId = await seedSession(request, auth)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.getByTestId('chat-toolbar-status')).toBeVisible({ timeout: 10_000 })

  // Turn 1 is fresh, so it works and leaves a conversation behind.
  const first = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth,
    data: { text: 'remember the word pelican', model: 'mock:context-overflow' },
  })
  expect(first.ok(), `send failed: ${await first.text()}`).toBeTruthy()
  await expect(page.getByText('Fresh conversation started.').first()).toBeVisible({
    timeout: 15_000,
  })

  // Turn 2 resumes that conversation, which no longer "fits".
  const second = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth,
    data: { text: 'what was the word?', model: 'mock:context-overflow' },
  })
  expect(second.ok(), `send failed: ${await second.text()}`).toBeTruthy()

  // The chat says what happened…
  const notice = page.getByTestId('chat-handover').filter({ hasText: NOTICE })
  await expect(notice).toBeVisible({ timeout: 20_000 })
  // …and the replayed turn is answered from a fresh context that got the
  // transcript.
  await expect(page.getByText('Recovered with a fresh context and the transcript.')).toBeVisible({
    timeout: 20_000,
  })

  // The carried-over history is there to inspect, and holds turn 1 but not
  // the replayed message (that one is sent as the user's message).
  await notice.locator('summary').click()
  await expect(notice.getByText('remember the word pelican')).toBeVisible()
  await expect(notice.getByText('what was the word?')).toHaveCount(0)

  const evs = await events(request, auth, sessionId)
  const handover = evs.find((e) => e.kind === 'handover')
  expect(handover?.data.overflow).toBe(true)
  // The replay must not duplicate the user's message in the transcript.
  expect(evs.filter((e) => e.kind === 'user')).toHaveLength(2)
})
