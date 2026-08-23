import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Recovery-mode account/provider switch.
 *
 * When the outgoing agent cannot write a handover doc (usage limit), the
 * user sends the reconstructed transcript to the incoming model instead.
 * The dialog must show the token count and estimated input cost before
 * confirming, and the switch must complete without a handover-start /
 * outgoing-agent turn.
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

async function loadApp(page: Page, token: string, route: string) {
  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto(route)
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })
}

type WsEvent = { kind: string; data: Record<string, unknown>; seq: number }

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

async function seedSession(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  model: string,
): Promise<{ sessionId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-recovery-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-recovery-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'recovery', folder_id: folder.id, model },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  return { sessionId: session.id }
}

test('recovery preview then recover injects the transcript without the outgoing agent', async ({
  request,
}) => {
  const { authHeader } = await authenticate(request)
  const { sessionId } = await seedSession(request, authHeader, 'mock:echo')

  {
    const send = await request.post(`/api/sessions/${sessionId}/message`, {
      headers: authHeader,
      data: { text: 'remember the purple widget' },
    })
    expect(send.ok(), `first send failed: ${await send.text()}`).toBeTruthy()
  }
  const afterFirst = maxSeq(
    await waitForEvent(request, authHeader, sessionId, 'agent-end', 0, 15_000),
  )

  const previewRes = await request.get(
    `/api/sessions/${sessionId}/recovery-preview?model=${encodeURIComponent('mock:echo@acct2')}`,
    { headers: authHeader },
  )
  expect(previewRes.ok(), `preview failed: ${await previewRes.text()}`).toBeTruthy()
  const preview = (await previewRes.json()) as {
    tokens: number
    est_cost_usd: number
    fits: boolean
    to_model: string
  }
  expect(preview.tokens).toBeGreaterThan(0)
  expect(preview.est_cost_usd).toBeGreaterThan(0)
  expect(preview.fits).toBe(true)
  expect(preview.to_model).toBe('mock:echo@acct2')

  const recoverRes = await request.post(`/api/sessions/${sessionId}/recover`, {
    headers: authHeader,
    data: { model: 'mock:echo@acct2' },
  })
  expect(recoverRes.ok(), `recover failed: ${await recoverRes.text()}`).toBeTruthy()
  const recovered = (await recoverRes.json()) as {
    model: string | null
    handover_to_model: string | null
  }
  expect(recovered.model).toBe('mock:echo@acct2')
  expect(recovered.handover_to_model).toBeNull()

  const events = (await (
    await request.get(`/api/sessions/${sessionId}/events?limit=1000`, { headers: authHeader })
  ).json()) as WsEvent[]
  expect(events.some((e) => e.kind === 'handover' && e.data.recovery === true)).toBeTruthy()
  expect(events.some((e) => e.kind === 'handover-start' && e.seq > afterFirst)).toBeFalsy()

  {
    const send = await request.post(`/api/sessions/${sessionId}/message`, {
      headers: authHeader,
      data: { text: 'continue please' },
    })
    expect(send.ok(), `post-recovery send failed: ${await send.text()}`).toBeTruthy()
    const after = await waitForEvent(
      request,
      authHeader,
      sessionId,
      'agent-end',
      maxSeq(events),
      15_000,
    )
    const joined = after
      .filter((e) => e.kind === 'agent-text')
      .map((e) => String(e.data.text ?? ''))
      .join('')
    expect(joined).toContain('Recovery context')
    expect(joined).toContain('remember the purple widget')
    expect(joined).toContain('continue please')
  }
})

test('switch dialog shows recovery token cost and sends the transcript', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedSession(request, authHeader, 'mock:echo')

  {
    const send = await request.post(`/api/sessions/${sessionId}/message`, {
      headers: authHeader,
      data: { text: 'keep the blue lantern' },
    })
    expect(send.ok(), `first send failed: ${await send.text()}`).toBeTruthy()
  }
  await waitForEvent(request, authHeader, sessionId, 'agent-end', 0, 15_000)

  await loadApp(page, token, `/sessions/${sessionId}`)

  const trigger = page.getByTestId('chat-toolbar-model')
  await expect(trigger).toBeVisible({ timeout: 10_000 })
  await trigger.click()
  await page.getByTestId('chat-toolbar-model-option-claude:claude-opus-4-8').click()

  const dialog = page.getByTestId('model-switch-prompt')
  await expect(dialog).toBeVisible()
  await expect(page.getByTestId('model-switch-recovery')).toBeVisible()

  await page.getByTestId('model-switch-recovery').click()
  const tokens = page.getByTestId('model-switch-recovery-tokens')
  await expect(tokens).toBeVisible({ timeout: 10_000 })
  await expect(tokens).toContainText('tokens')
  await expect(page.getByTestId('model-switch-recovery-cost')).toContainText('$')

  await page.getByTestId('model-switch-recovery-confirm').click()
  await expect(dialog).toHaveCount(0, { timeout: 10_000 })

  const after = await request.get(`/api/sessions/${sessionId}`, { headers: authHeader })
  const detail = (await after.json()) as {
    model: string | null
    handover_to_model: string | null
    pending_handover_doc: string | null
  }
  expect(detail.model).toBe('claude:claude-opus-4-8')
  expect(detail.handover_to_model).toBeNull()
  expect(detail.pending_handover_doc ?? '').toContain('keep the blue lantern')

  const events = (await (
    await request.get(`/api/sessions/${sessionId}/events?limit=1000`, { headers: authHeader })
  ).json()) as WsEvent[]
  expect(events.some((e) => e.kind === 'handover' && e.data.recovery === true)).toBeTruthy()
  expect(events.some((e) => e.kind === 'handover-start')).toBeFalsy()
})
