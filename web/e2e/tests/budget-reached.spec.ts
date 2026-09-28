import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Spend budget gate: a project whose spend in the current window has
 * reached its cap starts no new workers, but is never paused — pausing is
 * user-only. The board shows a "Budget reached" banner driven by the
 * derived `budget_exhausted` flag, and the banner clears (via the
 * orchestrator's project-update broadcast) once the budget is raised.
 *
 * A 0-cent cap is reached by definition, so no real spend is needed.
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

test('reached budget stops new workers without pausing the project', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-budget-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-budget-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: 'budget reached',
      folder_id: folder.id,
      worker_count: 1,
      workflow: 'task',
      model: 'mock:happy-path',
      budget_usd_cents: 0,
      budget_period: 'daily',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: auth,
    data: { title: 'Waiting on budget', description: '', step: 'backlog', priority: 1 },
  })
  expect(cardRes.ok(), `create card failed: ${await cardRes.text()}`).toBeTruthy()
  const cardId = ((await cardRes.json()) as { id: string }).id

  await loadAt(page, token, `/projects/${project.id}`)

  const banner = page.getByTestId('project-budget-banner')
  await expect(banner).toBeVisible({ timeout: 10_000 })
  await expect(banner).toContainText('Budget reached')
  // The board only shows a status badge for a non-active project.
  await expect(page.locator('.status-badge.status-paused')).toHaveCount(0)
  await expect(page.getByTestId('project-pause-banner')).toHaveCount(0)

  // Give the orchestrator more than one tick (~5s): the card must not be
  // picked up, and the project must stay active.
  await page.waitForTimeout(7_000)
  const getRes = await request.get(`/api/projects/${project.id}`, { headers: auth })
  expect(getRes.ok(), `get project failed: ${await getRes.text()}`).toBeTruthy()
  const body = (await getRes.json()) as {
    project: { status: string; budget_exhausted: boolean }
    cards: Array<{ id: string; step: string; worker_session_id: string | null }>
  }
  expect(body.project.status).toBe('active')
  expect(body.project.budget_exhausted).toBe(true)
  const card = body.cards.find((c) => c.id === cardId)
  expect(card, 'card present').toBeTruthy()
  expect(card!.worker_session_id).toBeNull()
  expect(card!.step).toBe('backlog')

  // Raise the budget out-of-band: the orchestrator notices the flip on its
  // next tick and broadcasts, so the banner clears without a reload.
  const raiseRes = await request.put(`/api/projects/${project.id}`, {
    headers: auth,
    data: { budget_usd_cents: 1_000_000 },
  })
  expect(raiseRes.ok(), `raise budget failed: ${await raiseRes.text()}`).toBeTruthy()
  await expect(banner).toBeHidden({ timeout: 15_000 })
  await expect(page.locator('.status-badge.status-paused')).toHaveCount(0)
})
