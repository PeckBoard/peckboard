import { test, expect, type APIRequestContext } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { WebSocketImpl, type WsMessageEvent } from './ws-compat'

/**
 * Peckboard-managed background tasks, end to end at the API/WS level.
 *
 * `mock:background` starts three tasks through the REAL `run_background`
 * MCP tool — "ok" (`echo hello`), "fail" (`false`), "long" (`sleep 30`) —
 * then ends its turn. Each task's exit appends a `user` event tagged
 * `source: "background-task"` to the session and wakes it; the mock answers
 * every report with `ack background <label> <status>`, which is how this
 * spec proves the session was actually resumed. "long" is stopped over
 * REST and must report STOPPED the same way.
 *
 * `run_background` shares `run_command`'s approval gate. A chat session
 * would prompt, so the host-wide bypass is switched on for the test and
 * always restored.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type Auth = { Authorization: string }

async function authenticate(request: APIRequestContext): Promise<{ token: string; auth: Auth }> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

type Task = {
  id: string
  label: string
  status: string
  exit_code: number | null
  stopping: boolean
}

type SessionEvent = {
  seq: number
  kind: string
  data: {
    text?: string
    source?: string
    background_task?: { id: string; label: string; status: string; exit_code: number | null }
  }
}

type BgFrame = { action: string; task: Task }

/** Subscribe to `sessionId` and record every `background_task` frame. */
async function watchBackgroundFrames(baseURL: string, token: string, sessionId: string) {
  const ws = new WebSocketImpl(baseURL.replace(/^http/, 'ws') + '/ws')
  const frames: BgFrame[] = []
  await new Promise<void>((resolve, reject) => {
    ws.addEventListener('open', () => resolve())
    ws.addEventListener('error', (err) => reject(new Error(`WS error: ${String(err)}`)))
  })
  ws.send(JSON.stringify({ type: 'auth', token }))
  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('WS auth_ok not received')), 10_000)
    const handler = (msg: WsMessageEvent) => {
      if (JSON.parse(String(msg.data)).type === 'auth_ok') {
        clearTimeout(timer)
        ws.removeEventListener('message', handler)
        resolve()
      }
    }
    ws.addEventListener('message', handler)
  })
  ws.addEventListener('message', (msg) => {
    const frame = JSON.parse(String(msg.data))
    if (frame.type === 'background_task' && frame.session_id === sessionId) {
      frames.push(frame.data as BgFrame)
    }
  })
  ws.send(JSON.stringify({ type: 'subscribe', session_id: sessionId }))
  return { frames, close: () => ws.close() }
}

test('mock:background starts tasks, reports each exit into the session, and wakes it', async ({
  request,
  baseURL,
}) => {
  test.setTimeout(90_000)
  const { token, auth } = await authenticate(request)

  const listTasks = async (sessionId: string) => {
    const res = await request.get(`/api/sessions/${sessionId}/background`, { headers: auth })
    expect(res.ok(), `list background failed: ${await res.text()}`).toBeTruthy()
    return ((await res.json()) as { tasks: Task[] }).tasks
  }
  const statusByLabel = async (sessionId: string) =>
    Object.fromEntries((await listTasks(sessionId)).map((t) => [t.label, t.status]))
  const events = async (sessionId: string) => {
    const res = await request.get(`/api/sessions/${sessionId}/events?after_seq=0`, {
      headers: auth,
    })
    expect(res.ok(), `list events failed: ${await res.text()}`).toBeTruthy()
    return (await res.json()) as SessionEvent[]
  }
  const agentTexts = async (sessionId: string) =>
    (await events(sessionId)).filter((e) => e.kind === 'agent-text').map((e) => e.data.text ?? '')
  const reports = async (sessionId: string) =>
    (await events(sessionId)).filter(
      (e) => e.kind === 'user' && e.data.source === 'background-task',
    )

  const prior = await request.get('/api/settings/tool-permissions', { headers: auth })
  expect(prior.ok()).toBeTruthy()
  const priorBypass = ((await prior.json()) as { bypass: boolean }).bypass
  const bypassRes = await request.put('/api/settings/tool-permissions', {
    headers: auth,
    data: { bypass: true },
  })
  expect(bypassRes.ok(), `enable bypass failed: ${await bypassRes.text()}`).toBeTruthy()

  let ws: { frames: BgFrame[]; close: () => void } | undefined
  try {
    const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-bg-'))
    const folderRes = await request.post('/api/folders', {
      headers: auth,
      data: { name: 'e2e-bg', path: folderPath },
    })
    expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
    const folder = (await folderRes.json()) as { id: string }

    const sessionRes = await request.post('/api/sessions', {
      headers: auth,
      // Pin the model on the session: the wake-up turn resolves its model
      // from the session row, not from the first message's override.
      data: { name: 'background tasks', folder_id: folder.id, model: 'mock:background' },
    })
    expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
    const session = (await sessionRes.json()) as { id: string }

    ws = await watchBackgroundFrames(baseURL!, token, session.id)
    await new Promise((r) => setTimeout(r, 250))

    const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
      headers: auth,
      data: { text: 'start the background tasks', model: 'mock:background' },
    })
    expect(sendRes.ok(), `send message failed: ${await sendRes.text()}`).toBeTruthy()

    // Turn 1: all three tasks start and the turn ends.
    await expect
      .poll(() => agentTexts(session.id), { timeout: 20_000 })
      .toContain('started 3/3 background tasks')
    expect((await listTasks(session.id)).map((t) => t.label)).toEqual(['ok', 'fail', 'long'])

    // "ok" and "fail" exit on their own; "long" keeps running.
    await expect
      .poll(() => statusByLabel(session.id), { timeout: 20_000 })
      .toEqual({ ok: 'succeeded', fail: 'failed', long: 'running' })

    // Each exit is appended to the session as a tagged user event …
    await expect
      .poll(async () => (await reports(session.id)).map((e) => e.data.background_task?.label), {
        timeout: 20_000,
      })
      .toEqual(expect.arrayContaining(['ok', 'fail']))
    const early = await reports(session.id)
    const okReport = early.find((e) => e.data.background_task?.label === 'ok')!
    const failReport = early.find((e) => e.data.background_task?.label === 'fail')!
    expect(okReport.data.background_task).toMatchObject({ status: 'succeeded', exit_code: 0 })
    expect(okReport.data.text).toMatch(/^\[background task "ok" \([^)]+\) finished: exit 0 after/)
    expect(okReport.data.text).toContain('hello')
    expect(failReport.data.background_task).toMatchObject({ status: 'failed', exit_code: 1 })
    expect(failReport.data.text).toMatch(/^\[background task "fail" \([^)]+\) FAILED: exit 1 after/)

    // … and wakes the session, which acknowledges both.
    await expect
      .poll(() => agentTexts(session.id), { timeout: 20_000 })
      .toEqual(
        expect.arrayContaining(['ack background ok succeeded', 'ack background fail failed']),
      )

    // The log route serves the task's captured output.
    const tasks = await listTasks(session.id)
    const ok = tasks.find((t) => t.label === 'ok')!
    const long = tasks.find((t) => t.label === 'long')!
    const logRes = await request.get(`/api/background/${ok.id}/log`, { headers: auth })
    expect(logRes.ok(), `log failed: ${await logRes.text()}`).toBeTruthy()
    const log = (await logRes.json()) as { task: Task; lines: string[] }
    expect(log.task.id).toBe(ok.id)
    expect(log.lines).toContain('hello')

    // Stop "long" over REST → STOPPED report + ack.
    const stopRes = await request.post(`/api/background/${long.id}/stop`, { headers: auth })
    expect(stopRes.ok(), `stop failed: ${await stopRes.text()}`).toBeTruthy()
    expect(((await stopRes.json()) as { task: Task }).task.stopping).toBe(true)

    await expect
      .poll(() => statusByLabel(session.id), { timeout: 20_000 })
      .toEqual({ ok: 'succeeded', fail: 'failed', long: 'stopped' })
    await expect
      .poll(
        async () =>
          (await reports(session.id)).find((e) => e.data.background_task?.label === 'long')?.data,
        { timeout: 20_000 },
      )
      .toMatchObject({
        text: expect.stringMatching(/^\[background task "long" \([^)]+\) STOPPED after/),
        background_task: { status: 'stopped' },
      })
    await expect
      .poll(() => agentTexts(session.id), { timeout: 20_000 })
      .toContain('ack background long stopped')

    // A finished task can't be stopped again.
    const again = await request.post(`/api/background/${long.id}/stop`, { headers: auth })
    expect(again.status()).toBe(409)

    // The WS carried a started + finished frame for every task.
    const seen = (action: string) =>
      ws!.frames
        .filter((f) => f.action === action)
        .map((f) => f.task.label)
        .sort()
    expect(seen('started')).toEqual(['fail', 'long', 'ok'])
    expect(seen('finished')).toEqual(['fail', 'long', 'ok'])
  } finally {
    ws?.close()
    // Host-wide setting — never leave it loosened for the rest of the suite.
    await request.put('/api/settings/tool-permissions', {
      headers: auth,
      data: { bypass: priorBypass },
    })
  }
})
