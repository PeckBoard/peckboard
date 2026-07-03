import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { WebSocketImpl, type WsMessageEvent } from './ws-compat'

/**
 * Regression test for the WS fan-out leak: the send decision used
 * "does ANY client subscribe to this session" instead of "did THIS
 * client subscribe", so every connected client received every
 * subscribed session's events. Two WS clients subscribe to two
 * different sessions; each must only see its own session's events.
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

/** Open a WS, authenticate, subscribe to `sessionId`, and collect every
 *  `event` frame (for ANY session) until `untilKind` arrives for
 *  `untilSession` or the timeout hits. */
function collectFrames(
  baseURL: string,
  token: string,
  subscribeTo: string,
  untilSession: string,
  untilKind: string,
  timeoutMs: number,
): Promise<{ sessionId: string; kind: string }[]> {
  const wsUrl = baseURL.replace(/^http/, 'ws') + '/ws'
  const ws = new WebSocketImpl(wsUrl)
  const collected: { sessionId: string; kind: string }[] = []

  return new Promise((resolve, reject) => {
    const finish = () => {
      ws.close()
      resolve(collected)
    }
    const timer = setTimeout(finish, timeoutMs)

    ws.addEventListener('error', (err) => {
      clearTimeout(timer)
      ws.close()
      reject(new Error(`WS error: ${String(err)}`))
    })
    ws.addEventListener('open', () => {
      ws.send(JSON.stringify({ type: 'auth', token }))
    })
    ws.addEventListener('message', (msg: WsMessageEvent) => {
      const frame = JSON.parse(String(msg.data))
      if (frame.type === 'auth_ok') {
        ws.send(JSON.stringify({ type: 'subscribe', session_id: subscribeTo }))
        return
      }
      if (frame.type !== 'event') return
      collected.push({ sessionId: frame.session_id, kind: frame.event.kind })
      if (frame.session_id === untilSession && frame.event.kind === untilKind) {
        clearTimeout(timer)
        finish()
      }
    })
  })
}

test('WS clients only receive events for sessions they subscribed to', async ({
  request,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-iso-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-iso', path: folderPath },
  })
  expect(folderRes.ok()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const mkSession = async (name: string) => {
    const res = await request.post('/api/sessions', {
      headers: authHeader,
      data: { name, folder_id: folder.id },
    })
    expect(res.ok()).toBeTruthy()
    return ((await res.json()) as { id: string }).id
  }
  const sessionA = await mkSession('iso A')
  const sessionB = await mkSession('iso B')

  // Client A watches session A (and is the completion signal); client B
  // watches session B and must never see session A's stream.
  const clientA = collectFrames(baseURL!, token, sessionA, sessionA, 'agent-end', 15_000)
  const clientB = collectFrames(baseURL!, token, sessionB, sessionA, 'agent-end', 6_000)

  // Give both sockets a beat to auth + subscribe before triggering events.
  await new Promise((r) => setTimeout(r, 500))

  const sendRes = await request.post(`/api/sessions/${sessionA}/message`, {
    headers: authHeader,
    data: { text: 'hello', model: 'mock:happy-path' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  const [framesA, framesB] = await Promise.all([clientA, clientB])

  // Subscriber sees the scripted run.
  expect(framesA.some((f) => f.sessionId === sessionA && f.kind === 'agent-end')).toBeTruthy()

  // Non-subscriber must see nothing from session A.
  const leaked = framesB.filter((f) => f.sessionId === sessionA)
  expect(leaked, `client B leaked frames: ${JSON.stringify(leaked)}`).toHaveLength(0)
})
