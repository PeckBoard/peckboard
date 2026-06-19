import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * End-to-end coverage for the user-visible contract of worker session
 * management:
 *
 *  1. POST /interrupt actually stops the agent (no zombie run).
 *  2. POST /message while an agent is running queues the message
 *     instead of spawning a second concurrent agent.
 *  3. After completion, the queued message is automatically drained
 *     into a fresh run (no manual re-send needed).
 *  4. After a short session completes, the agent does not spontaneously
 *     restart — exactly one agent-start per user-triggered run.
 *
 * Uses the mock provider (no `claude` CLI dependency).
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type AuthBundle = {
  token: string
  authHeader: { Authorization: string }
}

async function authenticate(request: APIRequestContext): Promise<AuthBundle> {
  // The server auto-bootstraps the admin from PECKBOARD_BOOTSTRAP_*
  // env vars at first start (see playwright.config.ts); we just log in.
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

type WsFrame =
  | {
      type: 'event'
      session_id: string
      event: { kind: string; data: Record<string, unknown>; seq: number }
    }
  | { type: 'queue'; session_id: string; data: { action: string; text?: string } }
  | { type: 'auth_ok' }

type CollectedEvent = { kind: string; data: Record<string, unknown>; seq: number }

/**
 * Open a WS connection, authenticate, subscribe to `sessionId`, and
 * invoke `onFrame` for every relevant frame. Returns a `{ stop, ws }`
 * pair the test can use to close the socket.
 */
async function openCollector(
  baseURL: string,
  token: string,
  sessionId: string,
  onFrame: (frame: WsFrame) => void,
): Promise<{ stop: () => void }> {
  const wsUrl = baseURL.replace(/^http/, 'ws') + '/ws'
  const ws = new WebSocket(wsUrl)

  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('WS handshake timed out')), 5_000)
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
    const timer = setTimeout(() => reject(new Error('auth_ok not received')), 5_000)
    const handler = (msg: MessageEvent) => {
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
  ws.addEventListener('message', (msg) => {
    const frame = JSON.parse(String(msg.data)) as WsFrame
    onFrame(frame)
  })

  return { stop: () => ws.close() }
}

function eventsFor(sessionId: string, sink: CollectedEvent[]): (frame: WsFrame) => void {
  return (frame) => {
    if (frame.type === 'event' && frame.session_id === sessionId) {
      sink.push(frame.event)
    }
  }
}

async function waitFor(
  predicate: () => boolean,
  timeoutMs: number,
  description: string,
): Promise<void> {
  const deadline = Date.now() + timeoutMs
  while (!predicate()) {
    if (Date.now() > deadline) {
      throw new Error(`Timed out waiting for: ${description}`)
    }
    await new Promise((r) => setTimeout(r, 25))
  }
}

async function setupSession(request: APIRequestContext, authHeader: { Authorization: string }) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-lifecycle-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-lifecycle', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'lifecycle smoke', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  return session
}

test('interrupt actually stops a blocking agent and emits agent-end', async ({
  request,
  baseURL,
}) => {
  expect(baseURL).toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const session = await setupSession(request, authHeader)

  const events: CollectedEvent[] = []
  const collector = await openCollector(baseURL!, token, session.id, eventsFor(session.id, events))

  try {
    // mock:ask blocks waiting for stdin — perfect to test interrupt.
    const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
      headers: authHeader,
      data: { text: 'please ask me', model: 'mock:ask' },
    })
    expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()
    const sendBody = (await sendRes.json()) as { status: string }
    expect(sendBody.status).toBe('started')

    // Wait for agent-start so we know the run is actually in flight.
    await waitFor(() => events.some((e) => e.kind === 'agent-start'), 5_000, 'agent-start')

    // Interrupt — must terminate the run.
    const interruptRes = await request.post(`/api/sessions/${session.id}/interrupt`, {
      headers: authHeader,
    })
    expect(interruptRes.status()).toBe(204)

    // Within a few seconds we should see BOTH the interrupt marker AND
    // the agent-end (crashed) event from the streaming task winding down.
    await waitFor(
      () =>
        events.some((e) => e.kind === 'interrupt') && events.some((e) => e.kind === 'agent-end'),
      5_000,
      'interrupt + agent-end',
    )

    // And the session must no longer be in an active 'working' state.
    const statusRes = await request.get(`/api/sessions/${session.id}/status`, {
      headers: authHeader,
    })
    expect(statusRes.ok()).toBeTruthy()
    const status = (await statusRes.json()) as { status: string }
    expect(['crashed', 'idle']).toContain(status.status)
  } finally {
    collector.stop()
  }
})

test('POST /message while busy queues and is auto-drained on completion', async ({
  request,
  baseURL,
}) => {
  expect(baseURL).toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const session = await setupSession(request, authHeader)

  const events: CollectedEvent[] = []
  const queueEvents: { action: string; text?: string }[] = []
  const collector = await openCollector(baseURL!, token, session.id, (frame) => {
    if (frame.type === 'event' && frame.session_id === session.id) {
      events.push(frame.event)
    } else if (frame.type === 'queue' && frame.session_id === session.id) {
      queueEvents.push(frame.data)
    }
  })

  try {
    // First message: a blocking ask — keeps the agent busy.
    const first = await request.post(`/api/sessions/${session.id}/message`, {
      headers: authHeader,
      data: { text: 'busy please', model: 'mock:ask' },
    })
    expect(first.ok()).toBeTruthy()
    expect(((await first.json()) as { status: string }).status).toBe('started')

    await waitFor(() => events.some((e) => e.kind === 'agent-start'), 5_000, 'agent-start')

    // Second message arrives while the first run is still busy. The
    // route MUST queue (not spawn a parallel agent).
    const second = await request.post(`/api/sessions/${session.id}/message`, {
      headers: authHeader,
      data: { text: 'queued payload', model: 'mock:echo' },
    })
    expect(second.ok()).toBeTruthy()
    const secondBody = (await second.json()) as { status: string }
    expect(secondBody.status).toBe('queued')

    // The queue WS event must arrive.
    await waitFor(() => queueEvents.some((q) => q.action === 'set'), 3_000, 'queue:set broadcast')

    // The persistent queue holds the message.
    const queueGet = await request.get(`/api/sessions/${session.id}/queue`, {
      headers: authHeader,
    })
    expect(queueGet.ok()).toBeTruthy()
    expect(((await queueGet.json()) as { text: string }).text).toBe('queued payload')

    // While the first agent is busy, exactly ONE agent-start so far.
    expect(events.filter((e) => e.kind === 'agent-start').length).toBe(1)

    // Now release the first agent by interrupting it. Drain should
    // fire automatically because the completion listener calls
    // drain_queue_for_session regardless of how the run ended.
    const interruptRes = await request.post(`/api/sessions/${session.id}/interrupt`, {
      headers: authHeader,
    })
    expect(interruptRes.status()).toBe(204)

    // Wait for: first agent-end → drained queue text echoed → second agent-end.
    await waitFor(
      () => events.filter((e) => e.kind === 'agent-end').length >= 2,
      8_000,
      'second agent-end (after queue drain)',
    )

    // The second agent's run must have echoed the queued text.
    const textValues = events
      .filter((e) => e.kind === 'agent-text')
      .map((e) => String((e.data as { text?: string }).text ?? ''))
    expect(
      textValues.includes('queued payload'),
      `expected 'queued payload' in agent-text events, got ${JSON.stringify(textValues)}`,
    ).toBe(true)

    // Persistent queue is empty after drain.
    const queueAfter = await request.get(`/api/sessions/${session.id}/queue`, {
      headers: authHeader,
    })
    expect(queueAfter.status()).toBe(404)
  } finally {
    collector.stop()
  }
})

test('completed session does not spontaneously restart', async ({ request, baseURL }) => {
  expect(baseURL).toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const session = await setupSession(request, authHeader)

  const events: CollectedEvent[] = []
  const collector = await openCollector(baseURL!, token, session.id, eventsFor(session.id, events))

  try {
    // Single short echo run.
    const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
      headers: authHeader,
      data: { text: 'one-shot', model: 'mock:echo' },
    })
    expect(sendRes.ok()).toBeTruthy()

    // Wait for clean completion.
    await waitFor(
      () =>
        events.some(
          (e) => e.kind === 'agent-end' && (e.data as { status?: string }).status === 'complete',
        ),
      5_000,
      'agent-end status=complete',
    )

    // Watch for 3 seconds — strictly longer than the orchestrator's
    // 5s tick AND any reasonable handler reschedule window. No new
    // agent-start should appear.
    const startsSoFar = events.filter((e) => e.kind === 'agent-start').length
    await new Promise((r) => setTimeout(r, 3_000))
    const startsAfter = events.filter((e) => e.kind === 'agent-start').length
    expect(
      startsAfter,
      `agent restarted unexpectedly: ${startsAfter} agent-start events vs ${startsSoFar} initially`,
    ).toBe(startsSoFar)
  } finally {
    collector.stop()
  }
})
