import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * A blocked card used to be near-invisible while collapsed (a 3px left
 * border), had no recovery action, and its reason was mislabelled on
 * won't-do cards.
 *
 * Locks in:
 * - the collapsed card header carries a "Blocked" chip titled with the reason,
 * - the card `...` menu offers Unblock, and it clears blocked + reason,
 * - the detail modal calls the field "Won't-do reason" on a won't-do card
 *   and "Block reason" everywhere else.
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

/** worker_count=0 keeps every card parked where it was filed, so the
 *  orchestrator can't race the assertions on blocked state. */
async function createProject(
  request: APIRequestContext,
  auth: Record<string, string>,
  slug: string,
): Promise<string> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-${slug}-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-${slug}-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: { name: slug, folder_id: folder.id, worker_count: 0, workflow: 'task' },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  return ((await projectRes.json()) as { id: string }).id
}

async function createCard(
  request: APIRequestContext,
  auth: Record<string, string>,
  projectId: string,
  data: Record<string, unknown>,
): Promise<string> {
  const res = await request.post(`/api/projects/${projectId}/cards`, {
    headers: auth,
    data: { description: 'e2e', step: 'backlog', priority: 2, workflow: 'task', ...data },
  })
  expect(res.ok(), `create card failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

test('collapsed blocked card shows a Blocked chip, and Unblock clears the state', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)
  const projectId = await createProject(request, auth, 'blocked-chip')
  const cardId = await createCard(request, auth, projectId, {
    title: 'Auto-blocked card',
    blocked: true,
    block_reason: 'worker crashed 3 times in a row',
  })

  await loadAt(page, token, `/projects/${projectId}`)

  const card = page.locator('.kanban-card').filter({ hasText: 'Auto-blocked card' })
  await expect(card).toBeVisible({ timeout: 10_000 })
  // The chip has to read while the card is collapsed — that is the whole
  // point of moving it out of the expand-only body.
  await expect(card).toHaveAttribute('data-expanded', 'false')
  const chip = card.getByTestId('card-blocked-chip')
  await expect(chip).toBeVisible()
  await expect(chip).toHaveText('Blocked')
  await expect(chip).toHaveAttribute('title', 'worker crashed 3 times in a row')

  await card.locator('.kanban-card-menu-btn').click()
  await page.getByTestId('card-menu-unblock').click()

  await expect(chip).toBeHidden({ timeout: 10_000 })
  await expect(card).not.toHaveClass(/blocked/)

  const res = await request.get(`/api/projects/${projectId}/cards`, { headers: auth })
  expect(res.ok(), `list cards failed: ${await res.text()}`).toBeTruthy()
  const cards = (await res.json()) as Array<{
    id: string
    blocked: boolean
    block_reason: string | null
  }>
  const updated = cards.find((c) => c.id === cardId)
  expect(updated, 'card present in list').toBeTruthy()
  expect(updated!.blocked).toBe(false)
  expect(updated!.block_reason ?? '').toBe('')
})

test('unblocked card offers no Unblock action', async ({ request, page, baseURL }) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)
  const projectId = await createProject(request, auth, 'no-unblock')
  await createCard(request, auth, projectId, { title: 'Healthy card' })

  await loadAt(page, token, `/projects/${projectId}`)

  const card = page.locator('.kanban-card').filter({ hasText: 'Healthy card' })
  await expect(card).toBeVisible({ timeout: 10_000 })
  await expect(card.getByTestId('card-blocked-chip')).toHaveCount(0)

  await card.locator('.kanban-card-menu-btn').click()
  await expect(page.getByTestId('card-menu-move')).toBeVisible()
  await expect(page.getByTestId('card-menu-unblock')).toHaveCount(0)
})

test("won't-do card labels its reason as Won't-do reason", async ({ request, page, baseURL }) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)
  const projectId = await createProject(request, auth, 'wont-do-label')
  // The worker path stores the give-up reason in `block_reason` without
  // setting `blocked` (orchestrator.rs, WorkerIntent::WontDo). Reproduce
  // that shape: reason first, then the terminal step — terminal cards
  // only accept step changes.
  const cardId = await createCard(request, auth, projectId, {
    title: 'Abandoned card',
    blocked: false,
    block_reason: 'premise was wrong, nothing to do',
  })
  const moveRes = await request.put(`/api/projects/${projectId}/cards/${cardId}`, {
    headers: auth,
    data: { step: 'wont_do' },
  })
  expect(moveRes.ok(), `move to wont_do failed: ${await moveRes.text()}`).toBeTruthy()

  await loadAt(page, token, `/projects/${projectId}`)

  const card = page.locator('.kanban-card').filter({ hasText: 'Abandoned card' })
  await expect(card).toBeVisible({ timeout: 10_000 })
  await card.locator('.kanban-card-menu-btn').click()
  await page.getByRole('menuitem', { name: 'View', exact: true }).click()

  const modal = page.locator('.modal').filter({ hasText: 'Abandoned card' })
  await expect(modal).toBeVisible({ timeout: 10_000 })
  await expect(modal.locator('.card-detail-label', { hasText: "Won't-do reason" })).toBeVisible()
  await expect(modal.locator('.card-detail-label', { hasText: 'Block reason' })).toHaveCount(0)
  await expect(modal).toContainText('premise was wrong, nothing to do')
})
