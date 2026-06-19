import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Auto-pause defense: when a card's worker crashes twice in a row, the
 * orchestrator pauses the owning project and surfaces a banner explaining
 * why. The mock provider's `crash` scenario crashes deterministically on
 * every spawn, so a project pointed at `mock:crash` with at least one
 * card hits the threshold in two orchestrator ticks (~5s each).
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

test('crashing worker pauses project and surfaces a reason banner', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-auto-pause-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-auto-pause-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  // Project pinned to `mock:crash`: every spawn emits a Crashed event with
  // reason="mock scenario crash" + stderr="simulated stderr".
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: 'auto pause',
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
  // again, tripping the PAUSE_AFTER_CRASHES=2 threshold.
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

  await loadAt(page, token, `/projects/${project.id}`)

  // Two orchestrator ticks + crash bookkeeping should land within 30s.
  // The banner shows the card title, crash count, and a stderr snippet.
  const banner = page.getByTestId('project-pause-banner')
  await expect(banner).toBeVisible({ timeout: 30_000 })
  await expect(banner).toContainText('Crashing task')
  await expect(banner).toContainText('2 times')
  await expect(banner).toContainText('simulated stderr')

  // The status badge in the toolbar flips to "paused" so the pause is
  // discoverable from the project list too, not just the banner.
  await expect(page.locator('.status-badge.status-paused')).toBeVisible()

  // Resume via the API and verify the banner disappears via the
  // project-update WS broadcast — no reload required.
  const resumeRes = await request.post(`/api/projects/${project.id}/resume`, { headers: auth })
  expect(resumeRes.ok(), `resume failed: ${await resumeRes.text()}`).toBeTruthy()
  await expect(banner).toBeHidden({ timeout: 10_000 })
})
