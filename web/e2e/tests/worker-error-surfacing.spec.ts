import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Failed agent turns must be visible, not dressed up as success:
 *
 *  - `mock:auth-error` ends the turn with a Completed agent-end that
 *    carries `error` + `errorKind: 'auth_expired'` (the shape the Claude
 *    provider emits for an is_error `result`, e.g. an expired login).
 *    The chat must show a red "Agent failed" row with a remediation link
 *    to Settings, the toolbar pill must read "Error" — and the green
 *    "Ready for your next message." line must NOT appear.
 *  - An agent-start following failed turns renders a compact "retry N"
 *    chip, so orchestrator respawns read as attempts instead of
 *    identical repeated blocks.
 *  - The kanban card face shows an Error chip (tooltip = the error text)
 *    when its worker session's latest agent turn ended in an error.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function seedSession(request: APIRequestContext, auth: Record<string, string>) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-err-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-err-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'error surfacing', folder_id: folder.id },
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

test('an errored turn shows a red failed row + remediation, not the green ready line', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const { sessionId } = await seedSession(request, auth)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  const status = page.getByTestId('chat-toolbar-status')
  await expect(status).toBeVisible({ timeout: 10_000 })

  const send = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth,
    data: { text: 'fail auth please', model: 'mock:auth-error' },
  })
  expect(send.ok(), `send failed: ${await send.text()}`).toBeTruthy()

  // The failed-turn row, in error styling, with the error text. `.first()`
  // throughout: auth recovery replays the turn once automatically, so a
  // scenario that fails every time produces two failed rows, not one.
  await expect(page.getByText('Agent failed').first()).toBeVisible({ timeout: 15_000 })
  await expect(
    page.getByText('OAuth session expired and could not be refreshed', { exact: false }).first(),
  ).toBeVisible()

  // Auth failures link the user to Settings → Providers & Accounts.
  const remedy = page.getByTestId('chat-crash-auth-remedy').first()
  await expect(remedy).toBeVisible()
  await expect(remedy.getByRole('link')).toHaveAttribute('href', '/settings/providers')

  // Header pill says Error (red dot), not Idle.
  await expect(status).toHaveText('Error')
  await expect(status.locator('.status-dot-crashed')).toBeVisible()

  // No green "all good" line under an error.
  await expect(page.getByText('Ready for your next message.')).toHaveCount(0)
})
test('an agent-start after a failed turn is annotated "retry 1"', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const { sessionId } = await seedSession(request, auth)

  // `mock:crash` rather than an auth failure: an auth failure is replayed
  // automatically (see auth-recovery.spec.ts), which would put a second
  // failed turn on the streak before the injected start below.
  const send = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth,
    data: { text: 'crash please', model: 'mock:crash' },
  })
  expect(send.ok(), `send failed: ${await send.text()}`).toBeTruthy()

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.getByText('Agent crashed')).toBeVisible({ timeout: 15_000 })

  // The respawn attempt: an agent-start following the errored agent-end.
  const injected = await request.post(`/api/sessions/${sessionId}/events`, {
    headers: auth,
    data: { kind: 'agent-start', data: { model: 'mock:echo' } },
  })
  expect(injected.ok(), `inject agent-start failed: ${await injected.text()}`).toBeTruthy()

  const chip = page.getByTestId('chat-retry-chip')
  await expect(chip).toBeVisible({ timeout: 10_000 })
  await expect(chip).toHaveText('retry 1')
})

test('the kanban card face shows an Error chip when the worker turn failed', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-err-board-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-err-board-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  // worker_count 0 keeps the orchestrator away; the card points its
  // worker at a session we drive by hand (same trick as the thought-
  // bubble spec).
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: 'error chip',
      folder_id: folder.id,
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: auth,
    data: { title: 'failing card', description: '', step: 'in_progress', priority: 1 },
  })
  expect(cardRes.ok(), `create card failed: ${await cardRes.text()}`).toBeTruthy()
  const card = (await cardRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'failing worker', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  const assignRes = await request.put(`/api/projects/${project.id}/cards/${card.id}`, {
    headers: auth,
    data: { worker_session_id: session.id },
  })
  expect(assignRes.ok(), `assign worker failed: ${await assignRes.text()}`).toBeTruthy()

  // The failure: a real errored turn via the mock provider, so the
  // agent-end carries `error` + `errorKind` exactly as the Claude
  // provider writes them.
  const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
    headers: auth,
    data: { text: 'fail auth please', model: 'mock:auth-error' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  // Wait for the errored agent-end to persist, so the fresh page load
  // below exercises the `last_worker_error` seed on the cards fetch
  // (not just the live WS path).
  const deadline = Date.now() + 15_000
  let seeded = false
  while (Date.now() < deadline && !seeded) {
    const res = await request.get(`/api/projects/${project.id}/cards`, { headers: auth })
    expect(res.ok()).toBeTruthy()
    const cards = (await res.json()) as Array<Record<string, unknown>>
    const row = cards.find((c) => c.id === card.id)
    seeded = typeof row?.last_worker_error === 'string'
    if (!seeded) await new Promise((r) => setTimeout(r, 250))
  }
  expect(seeded, 'cards fetch never seeded last_worker_error').toBeTruthy()

  // Fresh load renders the chip from the seed.
  await loadAppAt(page, token, `/projects/${project.id}`)
  const chip = page.getByTestId('card-error-chip')
  await expect(chip).toBeVisible({ timeout: 15_000 })
  await expect(chip).toHaveText('Error')
  await expect(chip).toHaveAttribute(
    'title',
    'Failed to authenticate: OAuth session expired and could not be refreshed',
  )
})
