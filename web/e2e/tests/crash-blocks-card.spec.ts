import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Crash-loop defense: when a card's worker crashes twice in a row, the
 * orchestrator blocks that CARD (reason on the card's Blocked chip) and
 * never pauses the project — pausing is user-only. The mock provider's
 * `crash` scenario crashes deterministically on every spawn, so a project
 * pointed at `mock:crash` with one card hits the threshold in two
 * orchestrator ticks (~5s each).
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
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

test('crashing worker blocks its card and the project keeps running', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-crash-block-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-crash-block-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  // Project pinned to `mock:crash`: every spawn emits a Crashed event with
  // reason="mock scenario crash" + stderr="simulated stderr".
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: 'crash block',
      folder_id: folder.id,
      worker_count: 1,
      workflow: 'task',
      model: 'mock:crash',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  // One card in backlog: the orchestrator picks it up on its next tick
  // (~5s), spawns the mock crash worker, sees the Crashed event, clears
  // worker_session_id; on the following tick it respawns and crashes
  // again, tripping the BLOCK_AFTER_CRASHES=2 threshold.
  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: auth,
    data: {
      title: 'Crashing task',
      description: '',
      step: 'backlog',
      priority: 1,
    },
  })
  expect(cardRes.ok(), `create card failed: ${await cardRes.text()}`).toBeTruthy()
  const cardId = ((await cardRes.json()) as { id: string }).id

  await loadAt(page, token, `/projects/${project.id}`)

  // Two orchestrator ticks + crash bookkeeping should land within 30s.
  // The card's Blocked chip carries the reason: title, crash count, and a
  // stderr snippet.
  const card = page.locator('.kanban-card').filter({ hasText: 'Crashing task' })
  const chip = card.getByTestId('card-blocked-chip')
  await expect(chip).toBeVisible({ timeout: 30_000 })
  await expect(chip).toHaveAttribute('title', /Crashing task/)
  await expect(chip).toHaveAttribute('title', /2 times/)
  await expect(chip).toHaveAttribute('title', /simulated stderr/)

  // The project is untouched: no pause banner and no paused status badge
  // (the board only shows a badge for a non-active project), and the
  // server agrees.
  await expect(page.getByTestId('project-pause-banner')).toHaveCount(0)
  await expect(page.locator('.status-badge.status-paused')).toHaveCount(0)

  const projRes = await request.get(`/api/projects/${project.id}`, { headers: auth })
  expect(projRes.ok(), `get project failed: ${await projRes.text()}`).toBeTruthy()
  const body = (await projRes.json()) as {
    project: { status: string; pause_reason: string | null }
    cards: Array<{ id: string; blocked: boolean; worker_session_id: string | null }>
  }
  expect(body.project.status).toBe('active')
  expect(body.project.pause_reason).toBeNull()
  const blocked = body.cards.find((c) => c.id === cardId)
  expect(blocked, 'card present').toBeTruthy()
  expect(blocked!.blocked).toBe(true)
  expect(blocked!.worker_session_id).toBeNull()
})
