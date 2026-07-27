import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { WebSocketImpl, type WsMessageEvent } from './ws-compat'

/**
 * WS reliability (seq/resume card):
 *
 *  - A burst of events gets strictly contiguous seq numbers — no gaps,
 *    no duplicates (atomic seq assignment).
 *  - A client that reconnects and sends `resume {last_seq}` receives the
 *    full backlog past that seq (paginated server-side, RESUME_PAGE
 *    batches) followed by `resume_complete`, in order.
 *
 * The lag→resync path (broadcast slot overflow) needs a deliberately
 * slow consumer and is covered by the server's own handling
 * (`ws/handler.rs`); driving it deterministically from Playwright is not
 * practical, so this spec pins the resume/backlog contract instead.
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
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-resume-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-resume', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'ws resume', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  return session.id
}

test('burst seqs are contiguous and resume replays the full backlog past last_seq', async ({
  request,
  baseURL,
}) => {
  test.slow() // seeds 600 events over HTTP
  expect(baseURL).toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)

  const TOTAL = 600
  for (let i = 1; i <= TOTAL; i++) {
    const res = await request.post(`/api/sessions/${sessionId}/events`, {
      headers: authHeader,
      data: { kind: 'agent-text', data: { text: `event ${i}` } },
    })
    expect(res.ok(), `inject ${i} failed: ${await res.text()}`).toBeTruthy()
  }

  // Contiguity over HTTP: seq 1..TOTAL with no gaps.
  const eventsRes = await request.get(`/api/sessions/${sessionId}/events?after_seq=0`, {
    headers: authHeader,
  })
  expect(eventsRes.ok()).toBeTruthy()
  const all = (await eventsRes.json()) as { seq: number }[]
  expect(all.length).toBe(TOTAL)
  for (let i = 1; i < all.length; i++) {
    expect(all[i].seq, `gap between ${all[i - 1].seq} and ${all[i].seq}`).toBe(all[i - 1].seq + 1)
  }

  // Resume from seq 50 — like a client reconnecting after a drop — must
  // deliver 51..TOTAL in order, then resume_complete.
  const LAST_SEEN = 50
  const wsUrl = `${baseURL!.replace(/^http/, 'ws')}/ws`
  const ws = new WebSocketImpl(wsUrl)
  const replayed: number[] = []
  try {
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('WS open timeout')), 10_000)
      ws.addEventListener('open', () => {
        clearTimeout(timer)
        resolve()
      })
      ws.addEventListener('error', (err) => {
        clearTimeout(timer)
        reject(new Error(`WS error: ${String(err)}`))
      })
    })

    ws.send(JSON.stringify({ type: 'auth', token }))
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('auth_ok timeout')), 10_000)
      const handler = (msg: WsMessageEvent) => {
        const frame = JSON.parse(String(msg.data))
        if (frame.type === 'auth_ok') {
          clearTimeout(timer)
          ws.removeEventListener('message', handler)
          resolve()
        }
      }
      ws.addEventListener('message', handler)
    })

    ws.send(JSON.stringify({ type: 'subscribe', session_id: sessionId }))
    ws.send(JSON.stringify({ type: 'resume', session_id: sessionId, last_seq: LAST_SEEN }))

    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error(`resume_complete not seen; got ${replayed.length} events`)),
        20_000,
      )
      ws.addEventListener('message', (msg: WsMessageEvent) => {
        const frame = JSON.parse(String(msg.data))
        if (frame.type === 'event' && frame.session_id === sessionId) {
          replayed.push((frame.event as { seq: number }).seq)
        } else if (frame.type === 'resume_complete' && frame.session_id === sessionId) {
          clearTimeout(timer)
          resolve()
        }
      })
    })
  } finally {
    ws.close()
  }

  expect(replayed.length).toBe(TOTAL - LAST_SEEN)
  expect(replayed[0]).toBe(LAST_SEEN + 1)
  expect(replayed[replayed.length - 1]).toBe(TOTAL)
  for (let i = 1; i < replayed.length; i++) {
    expect(replayed[i], 'replay must be gap-free and ordered').toBe(replayed[i - 1] + 1)
  }
})
