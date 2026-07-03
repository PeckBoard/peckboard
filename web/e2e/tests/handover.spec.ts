import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * End-to-end coverage for the model-switch handover.
 *
 * When a session's model changes across a provider/account boundary, the
 * outgoing model writes a handover doc and the incoming model reads it on
 * its first turn (see src/handover.rs). This drives the whole flow through
 * the real HTTP layer using the mock `echo` provider, which echoes
 * whatever text it's given — so the doc the outgoing model "writes" is the
 * handover prompt, and the injected preamble is visible in the incoming
 * model's first reply.
 *
 * `mock:echo` → `mock:echo@acct2` keeps the same provider+scenario but
 * changes the account, which is exactly the continuity-key change that
 * triggers a handover.
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

type WsEvent = { kind: string; data: Record<string, unknown>; seq: number }

/** Poll the events HTTP endpoint until an event of `untilKind` appears
 *  (the Node test runtime here has no WebSocket global — see the e2e
 *  backlog note — so we poll rather than subscribe). Resolves with the
 *  full ordered event list. */
async function waitForEvent(
  request: APIRequestContext,
  authHeader: { Authorization: string },
  sessionId: string,
  untilKind: string,
  afterSeq: number,
  timeoutMs: number,
): Promise<WsEvent[]> {
  const deadline = Date.now() + timeoutMs
  let last: WsEvent[] = []
  while (Date.now() < deadline) {
    const res = await request.get(`/api/sessions/${sessionId}/events?limit=1000`, {
      headers: authHeader,
    })
    if (res.ok()) {
      last = (await res.json()) as WsEvent[]
      if (last.some((e) => e.kind === untilKind && e.seq > afterSeq)) return last
    }
    await new Promise((r) => setTimeout(r, 150))
  }
  throw new Error(
    `Did not see '${untilKind}' (seq > ${afterSeq}) in ${timeoutMs}ms; got: ${last
      .map((e) => e.kind)
      .join(', ')}`,
  )
}

function maxSeq(events: WsEvent[]): number {
  return events.reduce((m, e) => Math.max(m, e.seq), 0)
}

test('cross-account model switch generates a handover the new model reads', async ({ request }) => {
  const { authHeader } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-handover-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-handover', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'handover', folder_id: folder.id, model: 'mock:echo' },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  // 1. First turn on the outgoing model so the session has history worth
  //    handing over.
  {
    const send = await request.post(`/api/sessions/${session.id}/message`, {
      headers: authHeader,
      data: { text: 'first message' },
    })
    expect(send.ok(), `first send failed: ${await send.text()}`).toBeTruthy()
  }
  const afterFirst = maxSeq(
    await waitForEvent(request, authHeader, session.id, 'agent-end', 0, 15_000),
  )

  // 2. Switch to a different account.
  const patchRes = await request.patch(`/api/sessions/${session.id}`, {
    headers: authHeader,
    data: { model: 'mock:echo@acct2' },
  })
  expect(patchRes.ok(), `patch failed: ${await patchRes.text()}`).toBeTruthy()
  const parked = (await patchRes.json()) as {
    model: string | null
    handover_to_model: string | null
  }
  // The PATCH parks the target without flipping the live model yet — the
  // outgoing model must stay selected to write the doc.
  expect(parked.handover_to_model).toBe('mock:echo@acct2')
  expect(parked.model).toBe('mock:echo')

  // The "messages are refused with 409 while the handover is pending" guard
  // is pinned by the Rust route test `message_during_pending_handover_is_409`
  // (tests/message_during_handover.rs). Probing it here raced the mock's
  // instant doc turn: the flag could clear between a GET pre-check and the
  // probe POST, and an accepted probe consumed the handover preamble that
  // step 4 below asserts on.

  const handoverEvents = await waitForEvent(
    request,
    authHeader,
    session.id,
    'handover',
    afterFirst,
    15_000,
  )
  const kinds = handoverEvents.filter((e) => e.seq > afterFirst).map((e) => e.kind)
  expect(kinds).toContain('handover-start')
  expect(kinds).toContain('handover')

  const handover = handoverEvents.find((e) => e.kind === 'handover')!
  expect(handover.data.to).toBe('mock:echo@acct2')
  expect(handover.data.from).toBe('mock:echo')
  // The mock echoes the doc-generation prompt back as the "doc"; assert it
  // captured real content, not the empty-doc fallback.
  expect(String(handover.data.doc)).toContain('HANDOVER document')

  // 3. The switch has finalized — the session now reports the new model and
  //    a clear handover flag.
  const afterRes = await request.get(`/api/sessions/${session.id}`, { headers: authHeader })
  const after = (await afterRes.json()) as {
    model: string | null
    handover_to_model: string | null
  }
  expect(after.model).toBe('mock:echo@acct2')
  expect(after.handover_to_model).toBeNull()

  // 4. First turn under the new model: the echo reply must contain the
  //    injected handover preamble, proving the doc was fed to the incoming
  //    model rather than lost.
  const afterHandover = maxSeq(handoverEvents)
  {
    const send = await request.post(`/api/sessions/${session.id}/message`, {
      headers: authHeader,
      data: { text: 'continue please' },
    })
    expect(send.ok(), `post-handover send failed: ${await send.text()}`).toBeTruthy()
    const events = await waitForEvent(
      request,
      authHeader,
      session.id,
      'agent-end',
      afterHandover,
      15_000,
    )
    const fresh = events.filter((e) => e.seq > afterHandover)

    const joined = fresh
      .filter((e) => e.kind === 'agent-text')
      .map((e) => String(e.data.text))
      .join('')
    expect(joined).toContain('[Handover context')
    expect(joined).toContain('continue please')

    // The persisted user event keeps the user's ORIGINAL text, not the
    // injected preamble — injection only rides on the bytes sent to the model.
    const userEvent = fresh.find((e) => e.kind === 'user')
    expect(userEvent?.data.text).toBe('continue please')
  }
})
