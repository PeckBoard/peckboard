import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Streaming fold + thinking UI (token-streaming and thinking cards):
 *
 *  - Consecutive `agent-text` chunks arriving over WS fold into ONE
 *    assistant bubble that grows in place — no bubble-per-chunk, no
 *    duplicated text after the turn ends.
 *  - `mock:thinking` renders the collapsed "Thought process" block with
 *    the thought text inside, exactly one final answer bubble.
 *  - While a turn is running, the live last-line preview of the streaming
 *    thought shows beside the working dots (`chat-thinking-live`).
 *
 * The Claude-side delta emission/dedupe (`--include-partial-messages`,
 * snapshot-vs-streamed suppression) is covered by Rust unit tests in
 * `provider/claude/process/parser.rs`; this spec pins the UI fold the
 * deltas land in, using the events-injection backdoor
 * (`POST /api/sessions/:id/events`) during a `mock:block` turn.
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
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-stream-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-stream', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'stream fold', folder_id: folder.id },
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

test('mock:thinking renders one collapsed thought block and one answer, no duplication', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-vrow').first())).toBeVisible({
    timeout: 10_000,
  })

  const send = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'think please', model: 'mock:thinking' },
  })
  expect(send.ok(), `send failed: ${await send.text()}`).toBeTruthy()

  const thought = page.getByTestId('chat-thinking-block')
  await expect(thought).toBeVisible({ timeout: 10_000 })
  await expect(thought).toHaveCount(1)
  await expect(thought).toContainText('Thought process')

  // The two Thinking chunks folded into the block's body.
  await thought.locator('summary').click()
  await expect(thought).toContainText('Let me reason about this.')

  // Exactly one answer bubble, no post-turn duplication of its text.
  await expect(page.getByText('The answer is 42.')).toHaveCount(1)
})

test('agent-text chunks fold into a single growing bubble; live thought preview shows while running', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-vrow').first())).toBeVisible({
    timeout: 10_000,
  })

  // Keep a turn running so the working indicator is live.
  const send = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'stay busy', model: 'mock:block' },
  })
  expect(send.ok(), `send failed: ${await send.text()}`).toBeTruthy()
  await expect(page.getByText('working…')).toBeVisible({ timeout: 10_000 })

  try {
    // Streamed thinking: the live last line shows beside the dots.
    const think = await request.post(`/api/sessions/${sessionId}/events`, {
      headers: authHeader,
      data: {
        kind: 'agent-thinking',
        data: { text: 'first thought line\nlatest thought line' },
      },
    })
    expect(think.ok(), `inject thinking failed: ${await think.text()}`).toBeTruthy()
    await expect(page.locator('.chat-thinking-live')).toHaveText('latest thought line', {
      timeout: 10_000,
    })

    // Two text chunks → one bubble containing the concatenation.
    for (const chunk of ['chunk one — ', 'chunk two, same bubble.']) {
      const res = await request.post(`/api/sessions/${sessionId}/events`, {
        headers: authHeader,
        data: { kind: 'agent-text', data: { text: chunk } },
      })
      expect(res.ok(), `inject text failed: ${await res.text()}`).toBeTruthy()
    }
    const merged = page.getByText('chunk one — chunk two, same bubble.')
    await expect(merged).toHaveCount(1, { timeout: 10_000 })
    // Neither chunk rendered as its own extra bubble.
    await expect(page.getByText('chunk one —', { exact: true })).toHaveCount(0)
  } finally {
    await request.post(`/api/sessions/${sessionId}/interrupt`, { headers: authHeader })
  }
})
