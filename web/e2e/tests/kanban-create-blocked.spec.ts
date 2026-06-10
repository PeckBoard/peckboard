import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Creating a card from the kanban "Add Card" modal must let the user
 * file it already-blocked, with a reason. Pre-fix the Blocked checkbox
 * was only rendered in edit mode, so the only way to file a blocked
 * card was create + edit — two round-trips for what should be one.
 *
 * Locks in:
 * - the checkbox + reason input render in create mode,
 * - the card lands blocked=true with the typed reason persisted, and
 * - the kanban shows the blocked affordance on the new card.
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

test('Add Card modal can file a card already blocked with a reason', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-create-blocked-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-create-blocked-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  // worker_count=0 keeps any new card parked in backlog so the
  // orchestrator doesn't race with the assertions on its blocked state.
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: `create blocked`,
      folder_id: folder.id,
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  await loadAt(page, token, `/projects/${project.id}`)

  await page.getByRole('button', { name: 'Add Card' }).click()
  const modal = page.locator('.modal').filter({ hasText: 'New Card' })
  await expect(modal).toBeVisible({ timeout: 10_000 })

  await modal.locator('input.form-input').first().fill('Pre-blocked card')
  await modal.locator('textarea.card-form-description').fill('needs human triage')

  // Toggle Blocked and supply a reason — the reason input only appears
  // when the checkbox is checked, so this exercises the conditional
  // render too. The label wraps the input + a <span>Blocked</span>,
  // which is enough for a click on the label to flip the input.
  const blockedLabel = modal.locator('label.form-checkbox-label').filter({ hasText: 'Blocked' })
  await blockedLabel.click()
  await modal.getByPlaceholder('Block reason...').fill('waiting on product review')

  await modal.getByRole('button', { name: 'Create Card' }).click()
  await expect(modal).toBeHidden({ timeout: 10_000 })

  // Verify the persisted state via the HTTP list — this confirms the
  // POST /api/projects/:id/cards body picked up blocked + block_reason.
  const listRes = await request.get(`/api/projects/${project.id}/cards`, { headers: auth })
  expect(listRes.ok(), `list cards failed: ${await listRes.text()}`).toBeTruthy()
  const cards = (await listRes.json()) as Array<{
    title: string
    blocked: boolean
    block_reason: string | null
    step: string
  }>
  const created = cards.find((c) => c.title === 'Pre-blocked card')
  expect(created, 'created card present in list').toBeTruthy()
  expect(created!.blocked).toBe(true)
  expect(created!.block_reason).toBe('waiting on product review')
  expect(created!.step).toBe('backlog')

  // The kanban card should also surface the blocked state. The class
  // marker matches the pattern used elsewhere in the app for blocked
  // cards.
  const backlog = page.locator('.kanban-column', { hasText: 'Backlog' })
  const card = backlog.locator('.kanban-card').filter({ hasText: 'Pre-blocked card' })
  await expect(card).toBeVisible({ timeout: 10_000 })
  await expect(card).toHaveClass(/blocked/)
})
