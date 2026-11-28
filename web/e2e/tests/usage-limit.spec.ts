import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

/**
 * Recovery from a provider usage limit ("You've hit your weekly limit ·
 * resets 8am (UTC)").
 *
 * `mock:echo` replies with exactly the text it was sent, and the server
 * recognises that reply as a limit notice, so a chat message that IS a limit
 * notice stops its own turn on a limit. The hold is on the whole mock
 * account, which is what lets a chat turn hold back a project's worker.
 *
 *  1. Chat: the turn reads as a limit stop (never "crashed"), is parked with
 *     a "Retry now" button, and is sent again by itself at the reset.
 *  2. Chat: "Retry now" sends it again at once.
 *  3. Board: a card whose model is held shows a waiting chip, the board a
 *     banner; the banner's "Retry now" drops the hold and the card gets its
 *     worker.
 *
 * The echo re-parks on every re-send, so each test deletes its session and
 * clears every hold afterwards; a hold left behind would stop every later
 * spec that runs a mock worker.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

// Screenshots for a visual check (light, dark, narrow). Written next to the
// repo; not asserted on.
const SHOTS = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  '../../../.tmp-limit-shots',
)

const limitNotice = (seconds: number) => `You've hit your weekly limit · resets in ${seconds}s`

type Auth = Record<string, string>
interface ApiEvent {
  kind: string
  data: Record<string, unknown> | string
}

const createdSessions: string[] = []
const createdProjects: string[] = []

test.afterEach(async ({ request }) => {
  const { auth } = await authenticate(request)
  while (createdProjects.length > 0) {
    await request.delete(`/api/projects/${createdProjects.pop()}`, { headers: auth })
  }
  // Sessions first: a parked echo turn would re-park (and re-hold the
  // account) the moment the hold is dropped.
  while (createdSessions.length > 0) {
    await request.delete(`/api/sessions/${createdSessions.pop()}`, { headers: auth })
  }
  const cleared = await request.post('/api/provider-limits/clear', { headers: auth, data: {} })
  expect(cleared.ok(), `clear holds failed: ${await cleared.text()}`).toBeTruthy()
  const left = await request.get('/api/provider-limits', { headers: auth })
  expect(((await left.json()) as { holds: unknown[] }).holds).toEqual([])
})

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } as Auth }
}

async function seedSession(request: APIRequestContext, auth: Auth) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-limit-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-limit-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'usage limit', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  createdSessions.push(session.id)
  return { folderId: folder.id, sessionId: session.id }
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

async function sendLimitNotice(
  request: APIRequestContext,
  auth: Auth,
  sessionId: string,
  seconds: number,
) {
  const send = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth,
    data: { text: limitNotice(seconds), model: 'mock:echo' },
  })
  expect(send.ok(), `send failed: ${await send.text()}`).toBeTruthy()
}

async function events(request: APIRequestContext, auth: Auth, sessionId: string) {
  const res = await request.get(`/api/sessions/${sessionId}/events`, { headers: auth })
  expect(res.ok()).toBeTruthy()
  const list = (await res.json()) as ApiEvent[]
  return list.map((e) => ({
    kind: e.kind,
    data: (typeof e.data === 'string' ? JSON.parse(e.data) : e.data) as Record<string, unknown>,
  }))
}

/** True once a `limit-resumed` with `trigger` is followed by a new `agent-start`. */
async function resentAfter(
  request: APIRequestContext,
  auth: Auth,
  sessionId: string,
  trigger: string,
) {
  const list = await events(request, auth, sessionId)
  const resumed = list.findIndex((e) => e.kind === 'limit-resumed' && e.data.trigger === trigger)
  return resumed >= 0 && list.slice(resumed + 1).some((e) => e.kind === 'agent-start')
}

async function shots(page: Page, name: string) {
  // Colours and layout animate on a theme or size change; let them settle
  // so the picture shows the end state.
  const shot = async (file: string) => {
    await page.waitForTimeout(600)
    await page.screenshot({ path: path.join(SHOTS, file) })
  }
  for (const scheme of ['light', 'dark'] as const) {
    await page.emulateMedia({ colorScheme: scheme })
    await shot(`${name}-${scheme}.png`)
  }
  await page.setViewportSize({ width: 380, height: 800 })
  await shot(`${name}-dark-380.png`)
  await page.emulateMedia({ colorScheme: 'light' })
  await shot(`${name}-light-380.png`)
  await page.emulateMedia({ colorScheme: null })
  await page.setViewportSize({ width: 1280, height: 720 })
}

test('a chat turn stopped by a usage limit is parked and sent again at the reset', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const { sessionId } = await seedSession(request, auth)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.getByTestId('chat-toolbar-status')).toBeVisible({ timeout: 10_000 })

  await sendLimitNotice(request, auth, sessionId, 8)

  // A limit stop, with the waiting line and its escape hatch: not a crash.
  const parked = page.getByTestId('chat-limit-parked').first()
  await expect(parked).toBeVisible({ timeout: 15_000 })
  await expect(parked).toContainText('Usage limit reached. This message will be sent again')
  await expect(parked).not.toContainText(/crash/i)
  await expect(page.getByTestId('chat-limit-stop').first()).toContainText(
    'Stopped: usage limit reached',
  )
  await expect(page.getByText(/crashed/i)).toHaveCount(0)
  await expect(page.getByText('Agent failed')).toHaveCount(0)

  const list = await events(request, auth, sessionId)
  const end = list.find((e) => e.kind === 'agent-end')
  expect(end?.data.errorKind).toBe('rate_limit')
  expect(list.some((e) => e.kind === 'limit-parked')).toBe(true)

  // No click: the reset releases the turn and the message goes out again.
  await expect
    .poll(async () => resentAfter(request, auth, sessionId, 'reset'), { timeout: 25_000 })
    .toBe(true)
  await expect(page.getByTestId('chat-limit-resumed').first()).toContainText(
    'Limit reset — continuing.',
  )
  // Re-sent, not re-typed: still one user message in the transcript.
  const after = await events(request, auth, sessionId)
  expect(after.filter((e) => e.kind === 'user')).toHaveLength(1)
})

test('Retry now sends a parked turn again at once', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const { sessionId } = await seedSession(request, auth)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.getByTestId('chat-toolbar-status')).toBeVisible({ timeout: 10_000 })

  await sendLimitNotice(request, auth, sessionId, 120)

  const retry = page.getByTestId('chat-limit-retry')
  await expect(retry).toHaveCount(1, { timeout: 15_000 })
  await expect(page.getByTestId('chat-limit-parked')).toBeVisible()
  await shots(page, 'chat-parked')

  await retry.click()
  await expect
    .poll(async () => resentAfter(request, auth, sessionId, 'manual'), { timeout: 15_000 })
    .toBe(true)
  await expect(page.getByTestId('chat-limit-resumed').first()).toContainText('Retrying now.')
  // The echo hits the limit again: a second park, and only that one still
  // offers the button.
  await expect(page.getByTestId('chat-limit-parked')).toHaveCount(2, { timeout: 15_000 })
  await expect(page.getByTestId('chat-limit-retry')).toHaveCount(1)
})

test('a card held back by a usage limit shows on the board, and Retry now starts it', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)

  // Hold the mock account, then drop the chat session that did it: its
  // parked echo would take the hold straight back when the board clears it.
  const { folderId, sessionId } = await seedSession(request, auth)
  await sendLimitNotice(request, auth, sessionId, 120)
  await expect
    .poll(
      async () => (await events(request, auth, sessionId)).some((e) => e.kind === 'limit-parked'),
      { timeout: 15_000 },
    )
    .toBe(true)
  const dropped = await request.delete(`/api/sessions/${sessionId}`, { headers: auth })
  expect(dropped.ok(), `delete session failed: ${await dropped.text()}`).toBeTruthy()
  createdSessions.length = 0
  const holds = await request.get('/api/provider-limits', { headers: auth })
  expect(((await holds.json()) as { holds: unknown[] }).holds).toHaveLength(1)

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: 'limit wait',
      folder_id: folderId,
      worker_count: 1,
      workflow: 'task',
      model: 'mock:happy-path',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }
  createdProjects.push(project.id)

  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: auth,
    data: { title: 'Held by the limit', description: '', step: 'backlog', priority: 1 },
  })
  expect(cardRes.ok(), `create card failed: ${await cardRes.text()}`).toBeTruthy()
  const cardId = ((await cardRes.json()) as { id: string }).id

  type ProjectBody = {
    project: { limit_waits?: { card_id: string; model: string; until: string; reason: string }[] }
    cards: {
      id: string
      step: string
      worker_session_id: string | null
      last_worker_session_id?: string | null
    }[]
  }
  const readProject = async () => {
    const res = await request.get(`/api/projects/${project.id}`, { headers: auth })
    expect(res.ok(), `get project failed: ${await res.text()}`).toBeTruthy()
    return (await res.json()) as ProjectBody
  }

  // The orchestrator's next tick (~5 s) skips the card and records the wait.
  await expect
    .poll(async () => (await readProject()).project.limit_waits?.map((w) => w.card_id) ?? [], {
      timeout: 30_000,
    })
    .toEqual([cardId])
  const held = (await readProject()).cards.find((c) => c.id === cardId)
  expect(held?.worker_session_id ?? null).toBeNull()

  await loadAppAt(page, token, `/projects/${project.id}`)
  const card = page.locator('.kanban-card').filter({ hasText: 'Held by the limit' })
  const chip = card.getByTestId('card-limit-wait')
  await expect(chip).toBeVisible({ timeout: 15_000 })
  await expect(chip).toContainText('Waiting for limit · resets')
  await expect(chip).toHaveAttribute('title', /weekly limit/)
  const banner = page.getByTestId('project-limit-banner')
  await expect(banner).toBeVisible()
  await expect(banner).toContainText('1 card is waiting for a usage limit to reset')
  await expect(banner).toContainText('resets')
  await shots(page, 'board-waiting')

  await banner.getByTestId('project-limit-retry').click()
  await expect(banner).toHaveCount(0, { timeout: 15_000 })
  await expect(chip).toHaveCount(0)

  // The hold is gone, so the card gets its worker.
  await expect
    .poll(
      async () => {
        const c = (await readProject()).cards.find((x) => x.id === cardId)
        return Boolean(c?.worker_session_id || c?.last_worker_session_id || c?.step !== 'backlog')
      },
      { timeout: 30_000 },
    )
    .toBe(true)
  expect((await readProject()).project.limit_waits ?? []).toEqual([])
})
