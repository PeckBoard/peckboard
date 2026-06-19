import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e test: the kanban board shows a transient "thought bubble" on a
 * card whenever its worker session emits an event, giving at-a-glance
 * visibility into what each worker is doing.
 *
 * Setup avoids the orchestrator's spawn timing: we create a card, point its
 * `worker_session_id` at a plain session we control, load the board, then
 * drive that session with `mock:happy-path` so events stream in while the
 * board is mounted and subscribed.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  // The server auto-bootstraps the admin from PECKBOARD_BOOTSTRAP_*
  // env vars at first start (see playwright.config.ts); we just log in.
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

test('a card shows a thought bubble while its worker emits events', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, auth } = await authenticate(request)

  // Folder must exist on disk and have a unique path (UNIQUE constraint).
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-bubble-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'e2e-bubble', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  // A paused project keeps the orchestrator from touching our card.
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: 'bubble project',
      folder_id: folder.id,
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: auth,
    data: { title: 'Bubble Card', description: '', step: 'backlog', priority: 2 },
  })
  expect(cardRes.ok(), `create card failed: ${await cardRes.text()}`).toBeTruthy()
  const card = (await cardRes.json()) as { id: string }

  // A plain session we drive by hand; the card points its worker at it.
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'bubble worker', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  const assignRes = await request.put(`/api/projects/${project.id}/cards/${card.id}`, {
    headers: auth,
    data: { worker_session_id: session.id },
  })
  expect(assignRes.ok(), `assign worker failed: ${await assignRes.text()}`).toBeTruthy()

  // Load the board; the card should render and the board should subscribe
  // to the worker session.
  await loadAt(page, token, `/projects/${project.id}`)
  await expect(page.locator('.kanban-card-title', { hasText: 'Bubble Card' })).toBeVisible({
    timeout: 10_000,
  })

  // No bubble before any events arrive.
  await expect(page.locator('.card-thought-bubble')).toHaveCount(0)

  // Give the WS subscribe a beat to land before the agent emits.
  await page.waitForTimeout(300)

  const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
    headers: auth,
    data: { text: 'go', model: 'mock:happy-path' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  // A bubble appears and settles on the terminal "Done" summary.
  const bubble = page.locator('.card-thought-bubble')
  await expect(bubble).toBeVisible({ timeout: 10_000 })
  await expect(bubble).toContainText('Done', { timeout: 10_000 })

  // It auto-dismisses ~5s after the last event, leaving the card bubble-free.
  await expect(bubble).toHaveCount(0, { timeout: 10_000 })
})
