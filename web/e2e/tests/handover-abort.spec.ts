import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * End-to-end coverage for an INTERRUPTED handover (fix #3).
 *
 * A cross-account model switch parks the target in `handover_to_model` and
 * dispatches a doc-generation turn on the OUTGOING model (see
 * src/handover.rs). If the user cancels that turn, the completion listener
 * routes the not-completed result to `abort_handover`, which clears ONLY the
 * parked target and leaves `model` + `conversation_id` untouched — the switch
 * simply doesn't happen and no context is lost.
 *
 * This drives the real UI: the handover banner's Cancel button
 * (`data-testid=handover-cancel`) calls `interruptSession`, and the aborted
 * turn must render a subtle "Switch cancelled" notice (the `handover-aborted`
 * DisplayItem) while the session stays on the original model.
 *
 * `mock:block` is the outgoing model precisely because its scenario blocks
 * until interrupted WITHOUT emitting a ControlRequest — so the doc-generation
 * turn stays in flight (and the composer's handover Cancel button stays
 * visible) long enough to cancel, and the interrupt lands it as a
 * not-completed completion.
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

type WsEvent = { kind: string; data: Record<string, unknown>; seq: number }

/** Poll the events endpoint until an event of `untilKind` appears (the Node
 *  test runtime has no WebSocket global — same approach as handover.spec.ts). */
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

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

test('cancelling a handover keeps the model and renders a "Switch cancelled" notice', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-handover-abort-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-handover-abort', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'handover-abort', folder_id: folder.id, model: 'mock:block' },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  // 1. First turn on the outgoing model so the session has history worth
  //    handing over (a handover is refused otherwise). mock:block blocks; we
  //    interrupt it to land an agent-end, leaving real history behind.
  {
    const send = await request.post(`/api/sessions/${session.id}/message`, {
      headers: authHeader,
      data: { text: 'first message' },
    })
    expect(send.ok(), `first send failed: ${await send.text()}`).toBeTruthy()
  }
  await waitForEvent(request, authHeader, session.id, 'agent-start', 0, 15_000)
  {
    const irq = await request.post(`/api/sessions/${session.id}/interrupt`, { headers: authHeader })
    expect(irq.ok(), `first interrupt failed: ${await irq.text()}`).toBeTruthy()
  }
  const afterFirst = maxSeq(
    await waitForEvent(request, authHeader, session.id, 'agent-end', 0, 15_000),
  )

  // Snapshot the conversation the handover must NOT lose.
  const before = (await (
    await request.get(`/api/sessions/${session.id}`, { headers: authHeader })
  ).json()) as { model: string | null; conversation_id: string | null }
  expect(before.model).toBe('mock:block')

  // 2. Load the app on this session so the handover banner and the aborted
  //    notice render for real.
  await loadAppAt(page, token, `/sessions/${session.id}`)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-bubble').first())).toBeVisible({
    timeout: 10_000,
  })

  // 3. Switch to a different account: parks the target and dispatches the
  //    doc-generation turn on the outgoing mock:block, which blocks.
  const patchRes = await request.patch(`/api/sessions/${session.id}`, {
    headers: authHeader,
    data: { model: 'mock:block@acct2' },
  })
  expect(patchRes.ok(), `patch failed: ${await patchRes.text()}`).toBeTruthy()
  const parked = (await patchRes.json()) as {
    model: string | null
    handover_to_model: string | null
  }
  expect(parked.handover_to_model).toBe('mock:block@acct2')
  expect(parked.model).toBe('mock:block')

  await waitForEvent(request, authHeader, session.id, 'handover-start', afterFirst, 15_000)

  // 4. Reflect the parked handover in the open tab. In the real UI the model
  //    picker's patchSession sets this straight from the PATCH response; here
  //    the switch was driven out-of-band (the mock catalogue has no @acct2
  //    entry to pick, and parking a handover isn't broadcast), so we hand
  //    ChatView the freshly parked session over the exact
  //    `peckboard:session-updated` channel its live-update listener consumes.
  //    Everything after this — the banner, the Cancel click, the interrupt,
  //    the abort — is the real flow.
  const parkedSession = await request
    .get(`/api/sessions/${session.id}`, { headers: authHeader })
    .then((r) => r.json())
  await page.evaluate((s: { id: string }) => {
    window.dispatchEvent(
      new CustomEvent('peckboard:session-updated', {
        detail: { type: 'session-updated', session_id: s.id, data: s },
      }),
    )
  }, parkedSession)

  // 5. The handover banner (with its Cancel button) now renders. Click Cancel
  //    — it calls interruptSession, which lands the blocked doc turn as a
  //    not-completed completion → abort_handover.
  const cancelBtn = page.locator('[data-testid="handover-cancel"]')
  await expect(cancelBtn).toBeVisible({ timeout: 10_000 })
  await cancelBtn.click()

  // 6. The aborted handover renders the subtle "Switch cancelled" notice.
  await expect(
    page.locator('.chat-agent-start-label').filter({ hasText: 'Switch cancelled' }),
  ).toBeVisible({ timeout: 10_000 })

  // 7. The abort kept the model AND did NOT drop the conversation — the whole
  //    point. finalize_handover nulls conversation_id (so the incoming model
  //    starts fresh on the doc); abort must leave it set so the outgoing
  //    model resumes with its context. (The mock mints a fresh conversation
  //    id per turn's Started, so we assert non-null rather than exact value;
  //    the byte-exact preservation is pinned by the integration test
  //    `abort_handover_keeps_model_and_context`.)
  const after = (await (
    await request.get(`/api/sessions/${session.id}`, { headers: authHeader })
  ).json()) as {
    model: string | null
    handover_to_model: string | null
    conversation_id: string | null
  }
  expect(after.model, 'model unchanged after abort').toBe('mock:block')
  expect(after.handover_to_model, 'parked target cleared').toBeNull()
  expect(after.conversation_id, 'conversation not dropped (finalize would null it)').not.toBeNull()

  // A handover-aborted event was recorded; no `handover` (finalize) event fired.
  const events = await request
    .get(`/api/sessions/${session.id}/events?limit=1000`, { headers: authHeader })
    .then((r) => r.json() as Promise<WsEvent[]>)
  expect(events.some((e) => e.kind === 'handover-aborted')).toBeTruthy()
  expect(events.some((e) => e.kind === 'handover' && e.seq > afterFirst)).toBeFalsy()
})
