import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * The Done column shows most-recently-finished cards first. Cards moved
 * to `done` later jump to the top of the column, regardless of priority
 * or insertion order. Other columns continue to sort by priority asc.
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

test('done column sorts by most-recently-finished first; live updates re-order', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-done-order-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-done-order-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: { name: `done order`, folder_id: folder.id, worker_count: 0, workflow: 'task' },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  // Three cards seeded in `in_progress` so we can finish them in a
  // controlled order. Same priority for all three to prove the sort
  // really is by completion time, not priority.
  const cardIds: string[] = []
  for (const title of ['Alpha', 'Bravo', 'Charlie']) {
    const res = await request.post(`/api/projects/${project.id}/cards`, {
      headers: auth,
      data: { title, description: '', step: 'in_progress', priority: 2 },
    })
    expect(res.ok(), `seed ${title} failed: ${await res.text()}`).toBeTruthy()
    const card = (await res.json()) as { id: string }
    cardIds.push(card.id)
  }

  // Finish two cards in a specific order: Alpha first, then Bravo.
  // Sleep between to guarantee the rfc3339 timestamps are distinct.
  for (const idx of [0, 1]) {
    const res = await request.put(`/api/projects/${project.id}/cards/${cardIds[idx]}`, {
      headers: auth,
      data: { step: 'done' },
    })
    expect(res.ok(), `move card ${idx} to done failed: ${await res.text()}`).toBeTruthy()
    await new Promise((r) => setTimeout(r, 30))
  }

  await loadAt(page, token, `/projects/${project.id}`)

  const doneCol = page.locator('.kanban-column', { hasText: 'Done' })
  await expect(doneCol.locator('.kanban-card')).toHaveCount(2, { timeout: 10_000 })

  // Initial paint: Bravo (finished later) leads Alpha.
  const initialTitles = await doneCol.locator('.kanban-card .kanban-card-title').allTextContents()
  expect(initialTitles.map((s) => s.trim())).toEqual(['Bravo', 'Alpha'])

  // Now move Charlie into Done via the API — the WS broadcast must
  // re-order the rendered column in place, putting Charlie at the top.
  await new Promise((r) => setTimeout(r, 30))
  const finishCharlie = await request.put(`/api/projects/${project.id}/cards/${cardIds[2]}`, {
    headers: auth,
    data: { step: 'done' },
  })
  expect(
    finishCharlie.ok(),
    `move Charlie to done failed: ${await finishCharlie.text()}`,
  ).toBeTruthy()

  await expect(doneCol.locator('.kanban-card')).toHaveCount(3, { timeout: 10_000 })
  await expect
    .poll(
      async () =>
        (await doneCol.locator('.kanban-card .kanban-card-title').allTextContents()).map((s) =>
          s.trim(),
        ),
      { timeout: 10_000 },
    )
    .toEqual(['Charlie', 'Bravo', 'Alpha'])
})
