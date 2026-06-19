import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Pickup-order regression coverage for the kanban column sort:
 *
 *   1. Among same-priority cards, the older one (lower `created_at`)
 *      queues first. A newly-created card at the same priority must
 *      land behind existing ones.
 *   2. A card whose dependencies aren't satisfied sinks to the bottom
 *      of the column. Marking the prerequisite as done moves the
 *      dependent back to its priority-ordered position.
 *
 * The list-cards API drives both the orchestrator's pickup decision
 * and the UI render order, so this also covers the user-visible "first
 * one first" rule in the column.
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

async function seedProject(
  request: APIRequestContext,
  auth: Record<string, string>,
  suffix: string,
): Promise<{ projectId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-pickup-${suffix}-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-pickup-${suffix}-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    // worker_count=0 keeps the orchestrator from claiming our seeded
    // cards mid-test and moving them out of backlog.
    data: {
      name: `pickup ${suffix}`,
      folder_id: folder.id,
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }
  return { projectId: project.id }
}

test('same-priority cards queue oldest first; newly-created cards go to the back', async ({
  request,
  baseURL,
  page,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)
  const { projectId } = await seedProject(request, auth, 'created-at')

  // Three cards at the same priority. Sleep briefly so the rfc3339
  // timestamps are distinct (resolution is sub-millisecond, but a
  // tiny gap removes any chance of the assertion racing).
  for (const title of ['First', 'Second', 'Third']) {
    const res = await request.post(`/api/projects/${projectId}/cards`, {
      headers: auth,
      data: { title, description: '', step: 'backlog', priority: 2 },
    })
    expect(res.ok(), `seed ${title} failed: ${await res.text()}`).toBeTruthy()
    await new Promise((r) => setTimeout(r, 15))
  }

  // API order: priority ASC, then created_at ASC. Older cards lead.
  const listRes = await request.get(`/api/projects/${projectId}/cards`, { headers: auth })
  expect(listRes.ok()).toBeTruthy()
  const list = (await listRes.json()) as { title: string }[]
  expect(list.map((c) => c.title)).toEqual(['First', 'Second', 'Third'])

  // UI render mirrors the same order in the Backlog column.
  await loadAt(page, token, `/projects/${projectId}`)
  const backlog = page.locator('.kanban-column', { hasText: 'Backlog' })
  await expect(backlog.locator('.kanban-card')).toHaveCount(3, { timeout: 10_000 })
  const titles = await backlog.locator('.kanban-card .kanban-card-title').allTextContents()
  expect(titles.map((s) => s.trim())).toEqual(['First', 'Second', 'Third'])
})

test('cards with unmet dependencies sink to the bottom of the column', async ({
  request,
  baseURL,
  page,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)
  const { projectId } = await seedProject(request, auth, 'deps')

  // Three cards in backlog: Blocker (will be a prerequisite for
  // Dependent), Dependent, and Solo (no deps). All same priority.
  const titles = ['Blocker', 'Dependent', 'Solo']
  const ids: Record<string, string> = {}
  for (const title of titles) {
    const res = await request.post(`/api/projects/${projectId}/cards`, {
      headers: auth,
      data: { title, description: '', step: 'backlog', priority: 2 },
    })
    expect(res.ok(), `seed ${title} failed: ${await res.text()}`).toBeTruthy()
    const card = (await res.json()) as { id: string }
    ids[title] = card.id
    await new Promise((r) => setTimeout(r, 15))
  }

  // Make Dependent depend on Blocker.
  const depRes = await request.put(`/api/projects/${projectId}/cards/${ids['Dependent']}`, {
    headers: auth,
    data: { depends_on: [ids['Blocker']] },
  })
  expect(depRes.ok(), `set deps failed: ${await depRes.text()}`).toBeTruthy()

  await loadAt(page, token, `/projects/${projectId}`)
  const backlog = page.locator('.kanban-column', { hasText: 'Backlog' })
  await expect(backlog.locator('.kanban-card')).toHaveCount(3, { timeout: 10_000 })

  // Order: ready cards (Blocker, Solo by created_at), then waiting
  // (Dependent — has unmet deps).
  await expect
    .poll(
      async () =>
        (await backlog.locator('.kanban-card .kanban-card-title').allTextContents()).map((s) =>
          s.trim(),
        ),
      { timeout: 10_000 },
    )
    .toEqual(['Blocker', 'Solo', 'Dependent'])
})
