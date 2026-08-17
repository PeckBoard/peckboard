import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Recovery from a turn that failed to authenticate.
 *
 * A 401 used to strand a session outright: the CLI reads its token at
 * spawn, so the live child kept presenting the revoked one, and nothing
 * replayed the message that dispatch had already consumed. Three flows
 * cover the fix:
 *
 *  1. `mock:auth-error-once` fails the first turn and works after — the
 *     session must heal with NO user action (park → automatic release →
 *     drain into a fresh process).
 *  2. `mock:auth-error` fails every turn — the message must end up parked
 *     rather than spinning, and the "Retry now" button in the auth banner
 *     must release it again.
 *  3. Updating a Claude account's credential releases the turns parked on
 *     THAT account and leaves other accounts' alone.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const PARKED_NOTICE = 'Turn paused, waiting for a working login.'
/**
 * Accounts created during a test, torn down after it. The whole suite
 * shares one server, and `claude-accounts.spec.ts` asserts the empty
 * state — leaving rows behind fails a spec that has nothing to do with
 * this one.
 */
const createdAccounts: string[] = []

test.afterEach(async ({ request }) => {
  const { auth } = await authenticate(request)
  while (createdAccounts.length > 0) {
    const id = createdAccounts.pop() as string
    // force: sessions in these tests still point at the account, and the
    // delete guard refuses an account with live references without it.
    await request.delete(`/api/claude-accounts/${id}?force=true`, { headers: auth })
  }
})

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function seedSession(request: APIRequestContext, auth: Record<string, string>) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-auth-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-auth-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'auth recovery', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  return { folderId: folder.id, sessionId: session.id }
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

async function eventKinds(
  request: APIRequestContext,
  auth: Record<string, string>,
  sessionId: string,
) {
  const res = await request.get(`/api/sessions/${sessionId}/events`, { headers: auth })
  expect(res.ok()).toBeTruthy()
  const events = (await res.json()) as { kind: string }[]
  return events.map((e) => e.kind)
}

async function queuedTexts(
  request: APIRequestContext,
  auth: Record<string, string>,
  sessionId: string,
) {
  const res = await request.get(`/api/sessions/${sessionId}/queue`, { headers: auth })
  expect(res.ok()).toBeTruthy()
  const body = (await res.json()) as { messages: { text: string }[] }
  return body.messages.map((m) => m.text)
}

test('a turn that fails to authenticate replays itself once, with no user action', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const { sessionId } = await seedSession(request, auth)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.getByTestId('chat-toolbar-status')).toBeVisible({ timeout: 10_000 })

  const send = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth,
    data: { text: 'first attempt 401s', model: 'mock:auth-error-once' },
  })
  expect(send.ok(), `send failed: ${await send.text()}`).toBeTruthy()

  // The failure is still reported — recovery doesn't hide it.
  await expect(page.getByText('Agent failed')).toBeVisible({ timeout: 15_000 })
  await expect(page.getByText(PARKED_NOTICE, { exact: false })).toBeVisible({ timeout: 15_000 })

  // …and then heals on its own: released, re-dispatched, answered.
  await expect(
    page.getByText('Retrying the turn under a fresh agent process.', { exact: false }),
  ).toBeVisible({ timeout: 15_000 })
  await expect(page.getByText('Authenticated on the retry.')).toBeVisible({ timeout: 15_000 })

  // The replay must not duplicate the user's message in the transcript.
  const kinds = await eventKinds(request, auth, sessionId)
  expect(kinds.filter((k) => k === 'user')).toHaveLength(1)
  expect(kinds).toContain('auth-parked')
  expect(kinds).toContain('auth-resumed')

  // Delivered means the queue is empty again.
  expect(await queuedTexts(request, auth, sessionId)).toEqual([])
})

test('a turn that keeps failing parks instead of spinning, and Retry now releases it', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const { sessionId } = await seedSession(request, auth)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.getByTestId('chat-toolbar-status')).toBeVisible({ timeout: 10_000 })

  const send = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth,
    data: { text: 'always 401', model: 'mock:auth-error' },
  })
  expect(send.ok(), `send failed: ${await send.text()}`).toBeTruthy()

  await expect(page.getByText(PARKED_NOTICE, { exact: false }).first()).toBeVisible({
    timeout: 15_000,
  })

  // The free retry is spent once per credential, so the turn settles as
  // parked instead of looping. The message is held, not lost.
  await expect
    .poll(async () => queuedTexts(request, auth, sessionId), { timeout: 15_000 })
    .toEqual(['always 401'])

  // The escape hatch on the auth banner releases it again.
  const retry = page.getByTestId('chat-crash-auth-retry').first()
  await expect(retry).toBeVisible()
  await retry.click()
  await expect(page.getByText('Retrying the paused turn.', { exact: false })).toBeVisible({
    timeout: 15_000,
  })
})

test('updating an account credential releases the turns parked on that account only', async ({
  request,
}) => {
  const { auth } = await authenticate(request)

  const account = async (name: string) => {
    const res = await request.post('/api/claude-accounts', {
      headers: auth,
      data: { name, kind: 'api_key', credential: `sk-e2e-${name}-${Date.now()}` },
    })
    expect(res.ok(), `create account failed: ${await res.text()}`).toBeTruthy()
    const id = ((await res.json()) as { id: string }).id
    createdAccounts.push(id)
    return id
  }
  const mine = await account(`auth-recovery-mine-${Date.now()}`)
  const other = await account(`auth-recovery-other-${Date.now()}`)

  // Two sessions, one bound to each account. The model is set before the
  // first turn, so the PATCH is a direct write rather than a handover.
  //
  // The QUEUED rows deliberately name `mock:echo`: the release is decided
  // from the SESSION's account binding, while delivery uses the queued
  // row's model — which keeps the drain offline instead of shelling out to
  // a real `claude` binary that isn't there.
  const park = async (accountId: string, text: string) => {
    const { sessionId } = await seedSession(request, auth)
    const patch = await request.patch(`/api/sessions/${sessionId}`, {
      headers: auth,
      data: { model: `claude:claude-opus-4-8@${accountId}` },
    })
    expect(patch.ok(), `patch model failed: ${await patch.text()}`).toBeTruthy()
    const queued = await request.post(`/api/sessions/${sessionId}/queue`, {
      headers: auth,
      data: { text, model: 'mock:echo' },
    })
    expect(queued.ok(), `queue failed: ${await queued.text()}`).toBeTruthy()
    const marker = await request.post(`/api/sessions/${sessionId}/events`, {
      headers: auth,
      data: { kind: 'auth-parked', data: { model: `claude:claude-opus-4-8@${accountId}` } },
    })
    expect(marker.ok(), `park marker failed: ${await marker.text()}`).toBeTruthy()
    return sessionId
  }
  const parkedOnMine = await park(mine, 'held for my account')
  const parkedOnOther = await park(other, 'held for the other account')

  // A rename with no new secret changes nothing — the release is gated on
  // the credential actually being replaced.
  const rename = await request.put(`/api/claude-accounts/${mine}`, {
    headers: auth,
    data: { name: 'renamed only' },
  })
  expect(rename.ok(), `rename failed: ${await rename.text()}`).toBeTruthy()
  expect(await eventKinds(request, auth, parkedOnMine)).not.toContain('auth-resumed')

  const rotate = await request.put(`/api/claude-accounts/${mine}`, {
    headers: auth,
    data: { name: 'renamed only', credential: `sk-e2e-rotated-${Date.now()}` },
  })
  expect(rotate.ok(), `rotate failed: ${await rotate.text()}`).toBeTruthy()

  await expect
    .poll(async () => eventKinds(request, auth, parkedOnMine), { timeout: 15_000 })
    .toContain('auth-resumed')
  await expect
    .poll(async () => queuedTexts(request, auth, parkedOnMine), { timeout: 15_000 })
    .toEqual([])

  // The other account's session is untouched: a new login for one account
  // is no reason to retry a turn that failed on a different one.
  expect(await eventKinds(request, auth, parkedOnOther)).not.toContain('auth-resumed')
  expect(await queuedTexts(request, auth, parkedOnOther)).toEqual(['held for the other account'])
})
