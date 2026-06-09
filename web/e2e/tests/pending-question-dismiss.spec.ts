import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Regression: a pending `question` event used to linger in the chat as
 * an active card even after the user typed past it and sent a new
 * message — confusing because the question card stayed open while the
 * agent moved on. `dispatch::send_message` now auto-appends
 * `question-resolved {rejected: true}` for every unresolved question
 * before persisting the user's new message.
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

test('sending a message auto-dismisses pending questions', async ({ request, baseURL }) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { authHeader } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-qdismiss-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-qdismiss', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'q-dismiss', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  // Plant a `question` event the way the ask_user MCP tool would.
  const qRes = await request.post(`/api/sessions/${session.id}/events`, {
    headers: authHeader,
    data: {
      kind: 'question',
      data: { questions: [{ question: 'Pick a color?', header: 'Setup' }] },
    },
  })
  expect(qRes.ok(), `seed question failed: ${await qRes.text()}`).toBeTruthy()
  const question = (await qRes.json()) as { id: string }

  // User sends a message instead of answering.
  const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
    headers: authHeader,
    data: { text: 'never mind, just do it', model: 'mock:happy-path' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  // The send path auto-appended a `question-resolved` for the planted
  // question id with `rejected: true`. Read it back from the event log.
  const eventsRes = await request.get(`/api/sessions/${session.id}/events`, {
    headers: authHeader,
  })
  expect(eventsRes.ok()).toBeTruthy()
  const events = (await eventsRes.json()) as { kind: string; data: Record<string, unknown> }[]

  const resolved = events.find(
    (e) =>
      e.kind === 'question-resolved' &&
      (e.data.question_id === question.id || e.data.questionId === question.id),
  )
  expect(resolved, 'question-resolved auto-emitted for the pending question').toBeTruthy()
  expect(resolved?.data.rejected).toBe(true)
})
