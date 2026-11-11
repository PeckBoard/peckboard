import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync, readFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Focus mode for chat sessions: while the user is talking to a chat, a
 * message another session sends it (session-control `send_message`) is
 * held instead of landing in the conversation. The chat shows a "held"
 * chip naming the sender; "Show now" delivers it as its own, labelled turn.
 * The Settings toggle turns it off. Release-on-idle and the voice
 * Assistant's exemption are covered by the Rust tests.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { token: string }).token
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto(route)
}

async function makeFolder(request: APIRequestContext, auth: Record<string, string>) {
  const res = await request.post('/api/folders', {
    headers: auth,
    data: {
      name: `e2e-focus-${Date.now()}`,
      path: mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-focus-')),
    },
  })
  expect(res.ok(), await res.text()).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function makeSession(
  request: APIRequestContext,
  auth: Record<string, string>,
  folderId: string,
  name: string,
) {
  const res = await request.post('/api/sessions', {
    headers: auth,
    // Pinned so the released turn runs on the mock, not the instance default.
    data: { name, folder_id: folderId, model: 'mock:echo' },
  })
  expect(res.ok(), await res.text()).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

/** A running sender session's MCP bearer token (mock:block keeps it alive). */
async function senderToken(
  request: APIRequestContext,
  auth: Record<string, string>,
  sessionId: string,
) {
  const res = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth,
    data: { text: 'go', model: 'mock:block' },
  })
  expect(res.ok(), await res.text()).toBeTruthy()
  const cfgPath = path.join(process.env.PECKBOARD_E2E_DATA_DIR!, 'worker-mcp', `${sessionId}.json`)
  await expect
    .poll(() => {
      try {
        readFileSync(cfgPath, 'utf8')
        return true
      } catch {
        return false
      }
    })
    .toBe(true)
  const cfg = JSON.parse(readFileSync(cfgPath, 'utf8')) as {
    mcpServers: { peckboard: { headers: { Authorization: string } } }
  }
  return cfg.mcpServers.peckboard.headers.Authorization.replace(/^Bearer /, '')
}

async function sendAsSession(
  request: APIRequestContext,
  mcpToken: string,
  target: string,
  text: string,
) {
  const res = await request.post('/mcp', {
    headers: { Authorization: `Bearer ${mcpToken}` },
    data: {
      jsonrpc: '2.0',
      id: 1,
      method: 'tools/call',
      params: { name: 'send_message', arguments: { session_id: target, text } },
    },
  })
  expect(res.ok()).toBeTruthy()
  return JSON.stringify(await res.json())
}

async function setFocusMode(
  request: APIRequestContext,
  auth: Record<string, string>,
  enabled: boolean,
) {
  const res = await request.put('/api/settings/focus-mode', { headers: auth, data: { enabled } })
  expect(res.ok(), await res.text()).toBeTruthy()
}

test.afterEach(async ({ request }) => {
  const auth = { Authorization: `Bearer ${await authenticate(request)}` }
  await setFocusMode(request, auth, true)
})

test('a message from another session waits while you talk, and Show now delivers it alone', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const auth = { Authorization: `Bearer ${token}` }
  const folder = await makeFolder(request, auth)
  const chat = await makeSession(request, auth, folder, 'PCB planning')
  const sender = await makeSession(request, auth, folder, 'MochiMask check')
  const mcp = await senderToken(request, auth, sender)

  // The user is talking to the chat.
  const talk = await request.post(`/api/sessions/${chat}/message`, {
    headers: auth,
    data: { text: 'lets plan the PCB', model: 'mock:echo' },
  })
  expect(talk.ok(), await talk.text()).toBeTruthy()

  const reply = await sendAsSession(request, mcp, chat, 'board check: all green')
  expect(reply).toMatch(/held\\?":true/)

  await loadAt(page, token, `/sessions/${chat}`)
  const chip = page.getByTestId('chat-held-chip')
  await expect(chip).toBeVisible({ timeout: 15_000 })
  await expect(chip).toContainText('1 message held from MochiMask check')
  await expect(page.getByText('board check: all green')).toHaveCount(0)

  await page.getByTestId('chat-held-release').click()
  await expect(chip).toHaveCount(0)
  await expect(page.getByText(/held while the user was talking to you/).first()).toBeVisible({
    timeout: 15_000,
  })
  await expect(page.getByText(/board check: all green/).first()).toBeVisible()
  const held = await request.get(`/api/sessions/${chat}/held`, { headers: auth })
  expect(((await held.json()) as { count: number }).count).toBe(0)
})

test('with Focus mode off in Settings, the message goes straight through', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const auth = { Authorization: `Bearer ${token}` }
  await loadAt(page, token, '/settings/chat')
  const section = page.getByTestId('focus-mode-section')
  await expect(section).toBeVisible({ timeout: 15_000 })
  await expect(page.getByTestId('focus-mode-on')).toHaveAttribute('aria-pressed', 'true')
  await page.getByTestId('focus-mode-off').click()
  await expect
    .poll(async () => {
      const res = await request.get('/api/settings/focus-mode', { headers: auth })
      return ((await res.json()) as { enabled: boolean }).enabled
    })
    .toBe(false)

  const folder = await makeFolder(request, auth)
  const chat = await makeSession(request, auth, folder, 'Focus off chat')
  const sender = await makeSession(request, auth, folder, 'Focus off sender')
  const mcp = await senderToken(request, auth, sender)
  await request.post(`/api/sessions/${chat}/message`, {
    headers: auth,
    data: { text: 'hello', model: 'mock:echo' },
  })
  const reply = await sendAsSession(request, mcp, chat, 'not held')
  expect(reply).not.toMatch(/held\\?":true/)
  const held = await request.get(`/api/sessions/${chat}/held`, { headers: auth })
  expect(((await held.json()) as { count: number }).count).toBe(0)
})
