import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E for fix #2 — interactive sessions are never auto-compacted; instead
 * the chat UI prompts the user (compact / clear / continue) once the
 * context window fills.
 *
 * Drives the deterministic `mock:ctx` provider scenario, which echoes the
 * message and emits a Usage event whose context occupancy is the message
 * parsed as a number. That lets a test push context past the 150k prompt
 * floor and the +20k reappear step deterministically, instead of trying to
 * generate 150k real tokens.
 *
 * Covered:
 * - an interactive session at >= 150k context shows `chat-context-prompt`;
 * - clicking Continue dismisses it, and it reappears once context grows a
 *   further 20k;
 * - a worker session never shows the banner even at the same occupancy (the
 *   `!is_worker` guard), and its context badge advertises 200k auto-compaction.
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

async function loadAppAt(page: Page, token: string, route: string) {
  // Inject the auth token before any app script runs so the React app boots
  // authenticated instead of redirecting to login.
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

async function seedFolder(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  tag: string,
): Promise<{ id: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-ctx-${tag}-`))
  const res = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-ctx-${tag}`, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as { id: string }
}

/** Push context on `sessionId` to `ctx` tokens via the mock:ctx scenario. */
async function pushContext(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  sessionId: string,
  ctx: number,
) {
  const res = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: String(ctx), model: 'mock:ctx' },
  })
  expect(res.ok(), `push context failed: ${await res.text()}`).toBeTruthy()
}

test('interactive session prompts to manage context, dismisses, and re-prompts at +20k', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const folder = await seedFolder(request, authHeader, 'interactive')

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'context prompt smoke', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  await loadAppAt(page, token, `/sessions/${session.id}`)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-bubble').first())).toBeVisible({
    timeout: 10_000,
  })

  const banner = page.getByTestId('chat-context-prompt')

  // Below the 150k floor: no prompt.
  await pushContext(request, authHeader, session.id, 140_000)
  await expect(page.getByTestId('chat-toolbar-context')).toContainText('140k', { timeout: 10_000 })
  await expect(banner).toBeHidden()

  // The interactive badge advertises the prompt, not auto-compaction.
  await expect(page.getByTestId('chat-toolbar-context')).toHaveAttribute(
    'title',
    /prompted to compact past 150k/,
  )

  // Cross the 150k floor: the prompt appears.
  await pushContext(request, authHeader, session.id, 160_000)
  await expect(banner).toBeVisible({ timeout: 10_000 })

  // Continue dismisses it and bumps the floor to 160k + 20k = 180k.
  await page.getByTestId('chat-context-continue').click()
  await expect(banner).toBeHidden()

  // Still under the new 180k floor — stays dismissed.
  await pushContext(request, authHeader, session.id, 170_000)
  await expect(page.getByTestId('chat-toolbar-context')).toContainText('170k', { timeout: 10_000 })
  await expect(banner).toBeHidden()

  // Crossing the +20k step re-prompts.
  await pushContext(request, authHeader, session.id, 185_000)
  await expect(banner).toBeVisible({ timeout: 10_000 })
})

async function waitForWorkerSession(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  projectId: string,
  cardId: string,
): Promise<string> {
  const deadline = Date.now() + 30_000
  while (Date.now() < deadline) {
    const res = await request.get(`/api/projects/${projectId}/cards`, { headers: authHeader })
    expect(res.ok(), `list cards failed: ${await res.text()}`).toBeTruthy()
    const cards = (await res.json()) as {
      id: string
      worker_session_id: string | null
      last_worker_session_id: string | null
    }[]
    const card = cards.find((c) => c.id === cardId)
    const sid = card?.worker_session_id ?? card?.last_worker_session_id ?? null
    if (sid) return sid
    await new Promise((r) => setTimeout(r, 500))
  }
  throw new Error('orchestrator never spawned a worker session within 30s')
}

test('worker session never shows the context prompt', async ({ request, page, baseURL }) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)
  const folder = await seedFolder(request, authHeader, 'worker')

  // A one-worker project on mock:ctx: the orchestrator dispatches the card
  // description (non-numeric) to the worker, so mock:ctx falls back to its
  // 160k default — the same occupancy that prompts an interactive session.
  const projectRes = await request.post('/api/projects', {
    headers: authHeader,
    data: {
      name: 'ctx worker guard',
      folder_id: folder.id,
      worker_count: 1,
      workflow: 'task',
      model: 'mock:ctx',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: authHeader,
    data: { title: 'Fill some context', description: 'do the thing', step: 'backlog', priority: 1 },
  })
  expect(cardRes.ok(), `create card failed: ${await cardRes.text()}`).toBeTruthy()
  const card = (await cardRes.json()) as { id: string }

  const workerSessionId = await waitForWorkerSession(request, authHeader, project.id, card.id)

  await loadAppAt(page, token, `/sessions/${workerSessionId}`)

  // The worker's context badge renders (proving occupancy is populated) and
  // advertises the 200k worker auto-compaction — but the manage-context
  // prompt must never appear for a worker.
  const badge = page.getByTestId('chat-toolbar-context')
  await expect(badge).toBeVisible({ timeout: 10_000 })
  await expect(badge).toHaveAttribute('title', /auto-compacts at 200k/)
  await expect(page.getByTestId('chat-context-prompt')).toBeHidden()
})
