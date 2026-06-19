import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Empty-column placeholder behaviour inside the kanban board.
 *
 * The board renders the same vertical-columns layout at every
 * viewport, so the older horizontal-strip / section-locked sticky
 * tests don't apply anymore; this file keeps the empty-state
 * assertion, which is still load-bearing because an empty column
 * with no placeholder collapses to a thin strip the user would have
 * trouble dropping cards into.
 *
 * Project is created with `worker_count: 0` so the orchestrator never
 * runs anything against our cards.
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

async function setupProject(
  request: APIRequestContext,
  auth: Record<string, string>,
  suffix: string,
  cardCount: number,
): Promise<{ projectId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-wrap-${suffix}-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-wrap-${suffix}-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: `wrap project ${suffix}`,
      folder_id: folder.id,
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  for (let i = 0; i < cardCount; i++) {
    const res = await request.post(`/api/projects/${project.id}/cards`, {
      headers: auth,
      data: { title: `Card ${i + 1}`, description: '', step: 'backlog', priority: 2 },
    })
    expect(res.ok(), `create card ${i} failed: ${await res.text()}`).toBeTruthy()
  }
  return { projectId: project.id }
}

test('empty rows render a placeholder so they do not collapse', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)
  // One card in backlog → every other row is empty.
  const { projectId } = await setupProject(request, auth, 'empty', 1)

  await loadAt(page, token, `/projects/${projectId}`)

  const reviewRow = page.locator('.kanban-column', { hasText: 'Review' })
  await expect(reviewRow.locator('.kanban-cards-empty')).toHaveText(/No cards in Review/, {
    timeout: 10_000,
  })
})
