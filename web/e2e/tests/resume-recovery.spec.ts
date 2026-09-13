import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Recovery from a conversation the provider refuses to resume.
 *
 * Every CLI-backed provider resumes by id and treats an id it cannot find
 * as a hard startup failure — `no rollout found for thread id …` from
 * Codex, `No conversation found with session ID …` from Claude. That used
 * to wedge the session permanently: the id was re-derived identically on
 * every attempt, so retrying, and even terminating the agent, hit the same
 * wall. Only clearing the session escaped it, at the cost of the whole
 * transcript.
 *
 * `mock:resume-error` reproduces it exactly — it fails whenever it is
 * handed a conversation to resume and succeeds whenever it isn't. So the
 * second turn of a session always fails, and the session must heal itself:
 * drop the id, say so, replay the turn cold.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const RESET_NOTICE = 'The earlier conversation could not be resumed'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function seedSession(request: APIRequestContext, auth: Record<string, string>) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-resume-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-resume-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'resume recovery', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  return { folderId: folder.id, sessionId: session.id }
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

async function session(
  request: APIRequestContext,
  auth: Record<string, string>,
  sessionId: string,
) {
  const res = await request.get(`/api/sessions/${sessionId}`, { headers: auth })
  expect(res.ok()).toBeTruthy()
  return (await res.json()) as { conversation_id: string | null }
}

async function events(request: APIRequestContext, auth: Record<string, string>, sessionId: string) {
  const res = await request.get(`/api/sessions/${sessionId}/events`, { headers: auth })
  expect(res.ok()).toBeTruthy()
  return (await res.json()) as { kind: string; data: Record<string, unknown> }[]
}

async function queuedTexts(
  request: APIRequestContext,
  auth: Record<string, string>,
  sessionId: string,
) {
  const res = await request.get(`/api/sessions/${sessionId}/queue`, { headers: auth })
  expect(res.ok()).toBeTruthy()
  const body = (await res.json()) as { messages: { text: string }[] }
  return body.messages.map((m) => m.text)
}

test('a conversation the provider will not resume is dropped, and the turn replays cold', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const { sessionId } = await seedSession(request, auth)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.getByTestId('chat-toolbar-status')).toBeVisible({ timeout: 10_000 })

  // Turn 1 is cold, so it works — and leaves a conversation id behind.
  const first = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth,
    data: { text: 'first turn', model: 'mock:resume-error' },
  })
  expect(first.ok(), `send failed: ${await first.text()}`).toBeTruthy()
  await expect(page.getByText('Started a fresh conversation.').first()).toBeVisible({
    timeout: 15_000,
  })
  const dead = (await session(request, auth, sessionId)).conversation_id
  expect(dead).toBeTruthy()

  // Turn 2 is dispatched with that id, which the provider rejects.
  const second = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth,
    data: { text: 'second turn', model: 'mock:resume-error' },
  })
  expect(second.ok(), `send failed: ${await second.text()}`).toBeTruthy()

  // The session says what happened rather than failing silently…
  await expect(page.getByText(RESET_NOTICE, { exact: false })).toBeVisible({ timeout: 20_000 })
  // …and answers the turn anyway, from a fresh conversation.
  await expect(page.getByText('Started a fresh conversation.')).toHaveCount(2, { timeout: 20_000 })

  const kinds = (await events(request, auth, sessionId)).map((e) => e.kind)
  expect(kinds).toContain('conversation-reset')
  // The replay must not duplicate the user's message in the transcript.
  expect(kinds.filter((k) => k === 'user')).toHaveLength(2)
  // Delivered means the queue drained.
  expect(await queuedTexts(request, auth, sessionId)).toEqual([])

  // The rejected id is gone for good: the session is on a new conversation,
  // not the one the provider refused. (If the reset had left it reachable —
  // on the row or in the event log — the cold replay would have failed the
  // same way instead of answering.)
  const after = (await session(request, auth, sessionId)).conversation_id
  expect(after).toBeTruthy()
  expect(after).not.toBe(dead)
})
