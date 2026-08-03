import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Stale-response guard in `fetchCards` (web/src/store/projects.ts).
 *
 * `cards` is a single global list and the board doesn't filter it by
 * `project_id`, so a slow response for the project you just left used to
 * overwrite the board of the project you switched to — the board titled B
 * showing A's cards, with every card action POSTing to
 * `/api/projects/B/cards/<A-card-id>` and 404ing.
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

/** Project + one backlog card, so each board has a card only it can show. */
async function seedProject(
  request: APIRequestContext,
  auth: Record<string, string>,
  name: string,
  cardTitle: string,
): Promise<string> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-switch-race-${name}-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-switch-race-${name}-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: { name: `switch race ${name}`, folder_id: folder.id, worker_count: 0, workflow: 'task' },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: auth,
    data: { title: cardTitle, description: '', step: 'backlog', priority: 2 },
  })
  expect(cardRes.ok(), `seed card failed: ${await cardRes.text()}`).toBeTruthy()
  return project.id
}

/** In-app navigation: pushState + synthetic popstate, the same shape the app
 *  uses in `lib/reports.ts`. A `page.goto` would reload and drop the in-flight
 *  request the race depends on. */
async function spaNavigate(page: Page, route: string) {
  await page.evaluate((p) => {
    window.history.pushState(null, '', p)
    window.dispatchEvent(new PopStateEvent('popstate'))
  }, route)
}

test("a slow card fetch for the project you left can't paint the project you switched to", async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const projectA = await seedProject(request, auth, 'a', 'Alpha only card')
  const projectB = await seedProject(request, auth, 'b', 'Bravo only card')

  const backlog = page.locator('.kanban-column', { hasText: 'Backlog' })

  await loadAt(page, token, `/projects/${projectB}`)
  await expect(backlog.locator('.kanban-card-title', { hasText: 'Bravo only card' })).toBeVisible({
    timeout: 10_000,
  })

  // A's card list answers late — later than B's, which is served normally.
  await page.route(`**/api/projects/${projectA}/cards`, async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 1500))
    await route.continue()
  })

  const staleResponse = page.waitForResponse(
    (res) => res.url().endsWith(`/api/projects/${projectA}/cards`),
    { timeout: 15_000 },
  )

  await spaNavigate(page, `/projects/${projectA}`)
  // Switch back before A's response can land.
  await page.waitForTimeout(300)
  await spaNavigate(page, `/projects/${projectB}`)

  await expect(backlog.locator('.kanban-card-title', { hasText: 'Bravo only card' })).toBeVisible({
    timeout: 10_000,
  })

  // The stale response arrives now; it must be discarded, not committed.
  await staleResponse
  await page.waitForTimeout(500)

  await expect(backlog.locator('.kanban-card-title', { hasText: 'Bravo only card' })).toBeVisible()
  await expect(page.locator('.kanban-card-title', { hasText: 'Alpha only card' })).toHaveCount(0)
  await expect(page).toHaveURL(new RegExp(`/projects/${projectB}$`))
})
