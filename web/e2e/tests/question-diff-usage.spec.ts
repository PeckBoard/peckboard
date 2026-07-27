import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * ControlRequest question card, parallel file-diff attach, usage chip,
 * user-bubble markdown, and feed a11y attributes:
 *
 *  - `mock:ask` renders the "Input needed" card; submitting the answer
 *    resolves it by requestId and the provider receives the answer over
 *    stdin (its follow-up text lands in the feed).
 *  - Two edit_file tool cards open in parallel each attach the
 *    `file-diff` that matches their path, even when the diffs arrive in
 *    reverse order.
 *  - The turn-usage chip renders cost, k-formatted output tokens,
 *    context size with signed delta, and a duration once the turn spans
 *    ≥1s.
 *  - A markdown user message renders as rich markdown inside the user
 *    bubble; the feed root carries role="log" + aria-live="polite".
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
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-qdu-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-qdu', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'qdu verify', folder_id: folder.id },
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

test('mock:ask question card answers by requestId and the provider sees the answer', async ({
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
    data: { text: 'ask me', model: 'mock:ask' },
  })
  expect(send.ok()).toBeTruthy()

  const card = page.locator('.question-card.question-active')
  await expect(card).toBeVisible({ timeout: 10_000 })
  await expect(card).toContainText('Input needed')
  await expect(card).toContainText('Continue?')

  await card.getByPlaceholder('Type your answer...').fill('yes go ahead')
  await card.getByRole('button', { name: 'Submit' }).click()

  // Resolved card replaces the active one; the mock echoes the answer it
  // received over stdin, proving the requestId round-trip.
  await expect(page.locator('.question-card.question-resolved')).toContainText(
    'Question answered',
    { timeout: 10_000 },
  )
  await expect(page.getByText(/User answered: .*yes go ahead/)).toBeVisible({ timeout: 10_000 })

  // The persisted resolution references the original ControlRequest id.
  const eventsRes = await request.get(`/api/sessions/${sessionId}/events?after_seq=0`, {
    headers: authHeader,
  })
  const all = (await eventsRes.json()) as {
    kind: string
    data: { requestId?: string; request_id?: string }
  }[]
  const q = all.find((e) => e.kind === 'question')
  const resolved = all.find((e) => e.kind === 'question-resolved')
  expect(q?.data.requestId).toBeTruthy()
  expect(resolved?.data.request_id).toBe(q!.data.requestId)
})

test('parallel edit_file cards attach the file-diff matching their own path', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)

  await injectEvent(request, authHeader, sessionId, 'agent-tool-start', {
    toolUseId: 'tu-a',
    name: 'mcp__peckboard__edit_file',
    input: { path: 'src/parallel-a.ts' },
  })
  await injectEvent(request, authHeader, sessionId, 'agent-tool-start', {
    toolUseId: 'tu-b',
    name: 'mcp__peckboard__edit_file',
    input: { path: 'src/parallel-b.ts' },
  })
  // Diffs arrive in REVERSE order while both tools are still open.
  await injectEvent(request, authHeader, sessionId, 'file-diff', {
    path: 'src/parallel-b.ts',
    diff: '@@ -1 +1 @@\n-old b\n+new b',
    added: 1,
    removed: 1,
  })
  await injectEvent(request, authHeader, sessionId, 'file-diff', {
    path: 'src/parallel-a.ts',
    diff: '@@ -1 +1 @@\n-old a\n+new a',
    added: 1,
    removed: 1,
  })
  await injectEvent(request, authHeader, sessionId, 'agent-tool-end', {
    toolUseId: 'tu-a',
    output: 'edited a',
  })
  await injectEvent(request, authHeader, sessionId, 'agent-tool-end', {
    toolUseId: 'tu-b',
    output: 'edited b',
  })

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  // Each card carries exactly its own diff toggle.
  await expect(page.getByRole('button', { name: /Diff src\/parallel-a\.ts/ })).toBeVisible({
    timeout: 10_000,
  })
  await expect(page.getByRole('button', { name: /Diff src\/parallel-b\.ts/ })).toBeVisible()

  await page.getByRole('button', { name: /Diff src\/parallel-a\.ts/ }).click()
  await expect(page.getByText('+new a')).toBeVisible()
  await expect(page.getByText('+new b')).not.toBeVisible()
})

test('usage chip: cost, tokens, context delta, and duration; markdown user bubble; feed a11y', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-vrow').first())).toBeVisible({
    timeout: 10_000,
  })

  // Feed a11y: the scroller is a live log region.
  const feed = page.locator('.chat-messages[role="log"]')
  await expect(feed).toHaveAttribute('aria-live', 'polite')

  // Markdown user bubble.
  const send1 = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: '**bold user text** and `a span`', model: 'mock:usage' },
  })
  expect(send1.ok()).toBeTruthy()
  await expect(page.locator('.chat-user-markdown strong')).toHaveText('bold user text', {
    timeout: 10_000,
  })

  // First usage chip: mock:usage reports 400 out / 1500 ctx.
  const chip = page.locator('.chat-turn-usage')
  await expect(chip.first()).toContainText('400 tok out', { timeout: 10_000 })
  await expect(chip.first()).toContainText('1.5k ctx')

  // Second turn with a bigger context → the chip shows a signed delta.
  const send2 = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: '2600', model: 'mock:ctx' },
  })
  expect(send2.ok()).toBeTruthy()
  await expect(chip.nth(1)).toContainText('2.6k ctx (+1.1k)', { timeout: 10_000 })

  // Duration renders once the usage lands ≥1s after the turn anchor:
  // inject a late usage event (its server ts is 'now', well after the
  // last user event).
  // Duration only renders when the usage ts is ≥1s past the turn anchor
  // (the last user event) — give the anchor a real gap first.
  await page.waitForTimeout(1500)
  await injectEvent(request, authHeader, sessionId, 'agent-usage', {
    model: 'mock:ctx',
    inputTokens: 10,
    outputTokens: 2500,
    cacheReadTokens: 0,
    cacheCreationTokens: 0,
    totalTokens: 2510,
    contextTokens: 4000,
    turnSeq: 99,
  })
  await expect(chip.nth(2)).toContainText('2.5k tok out', { timeout: 10_000 })
  await expect(chip.nth(2)).toContainText(/\d+s/)
})
