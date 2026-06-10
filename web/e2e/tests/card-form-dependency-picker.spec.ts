import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * The card form's "Depends On" field opens a dedicated picker modal —
 * a button that surfaces backlog + running cards by default, with a
 * search box that widens the result set to every candidate (including
 * already-done cards).
 *
 * This test exercises the UI end-to-end:
 *   - the picker opens from the form,
 *   - backlog + running cards show by default, done cards are hidden,
 *   - typing into the search box reveals the done card,
 *   - the selected dependencies render as chips in the form,
 *   - submitting the form persists `depends_on` to the new card.
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

test('card form opens a dependency picker that filters by step and supports search', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-dep-picker-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-dep-picker-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  // worker_count=0 keeps cards parked where we put them — no
  // orchestrator transitions racing the picker assertions.
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: 'dep picker',
      folder_id: folder.id,
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  // Seed three cards spanning the three step buckets the picker cares
  // about: backlog (default-visible), in_progress (default-visible,
  // displayed as "Running"), and done (search-only).
  const seedCard = async (title: string, step: string) => {
    const res = await request.post(`/api/projects/${project.id}/cards`, {
      headers: auth,
      data: { title, description: '', step, priority: 2 },
    })
    expect(res.ok(), `seed ${title} failed: ${await res.text()}`).toBeTruthy()
    return (await res.json()) as { id: string }
  }
  const backlogCard = await seedCard('Backlog Prereq', 'backlog')
  const runningCard = await seedCard('Running Prereq', 'in_progress')
  const doneCard = await seedCard('Already Done', 'done')

  await loadAt(page, token, `/projects/${project.id}`)

  await page.getByRole('button', { name: 'Add Card' }).click()
  const formModal = page.locator('.modal').filter({ hasText: 'New Card' })
  await expect(formModal).toBeVisible({ timeout: 10_000 })

  await formModal.locator('input.form-input').first().fill('Needs Prereqs')

  // Open the picker. The trigger replaces the old inline checkbox list.
  await formModal.getByRole('button', { name: /Select Dependencies/i }).click()
  const picker = page.locator('.dependency-picker-modal')
  await expect(picker).toBeVisible({ timeout: 5_000 })

  // Default scope: backlog + running visible; done hidden.
  const optionTitle = (title: string) =>
    picker.locator('.dependency-picker-option', { hasText: title })
  await expect(optionTitle('Backlog Prereq')).toBeVisible()
  await expect(optionTitle('Running Prereq')).toBeVisible()
  await expect(optionTitle('Already Done')).toHaveCount(0)

  // Searching widens the set — typing the done card's title reveals it.
  const search = picker.getByPlaceholder('Search cards...')
  await search.fill('Already')
  await expect(optionTitle('Already Done')).toBeVisible()
  // The backlog card no longer matches the query, so it drops out
  // (unless it's already selected — it isn't yet).
  await expect(optionTitle('Backlog Prereq')).toHaveCount(0)

  // Select the done card from the filtered list, then clear the search
  // and pick the backlog card too. The done card stays selected and
  // therefore stays visible even though it falls outside the default
  // scope.
  await optionTitle('Already Done').locator('input[type="checkbox"]').check()
  await search.fill('')
  await expect(optionTitle('Already Done')).toBeVisible()
  await optionTitle('Backlog Prereq').locator('input[type="checkbox"]').check()

  // Confirm — the picker closes and the form shows chips for both.
  await picker.getByRole('button', { name: /Save \(2\)/ }).click()
  await expect(picker).toBeHidden({ timeout: 5_000 })
  const chips = formModal.locator('.dependency-chip')
  await expect(chips).toHaveCount(2)
  await expect(chips.filter({ hasText: 'Backlog Prereq' })).toBeVisible()
  await expect(chips.filter({ hasText: 'Already Done' })).toBeVisible()
  // The trigger now reflects the count and lets the user edit.
  await expect(formModal.getByRole('button', { name: /Edit Dependencies \(2\)/ })).toBeVisible()

  // Remove a chip — it disappears from the form, and the trigger
  // re-counts.
  await chips.filter({ hasText: 'Already Done' }).locator('.dependency-chip-remove').click()
  await expect(formModal.locator('.dependency-chip')).toHaveCount(1)
  await expect(formModal.getByRole('button', { name: /Edit Dependencies \(1\)/ })).toBeVisible()

  // Submit the form. The created card should record the remaining
  // dependency on the backlog card.
  await formModal.getByRole('button', { name: 'Create Card' }).click()
  await expect(formModal).toBeHidden({ timeout: 10_000 })

  const listRes = await request.get(`/api/projects/${project.id}/cards`, { headers: auth })
  expect(listRes.ok(), `list cards failed: ${await listRes.text()}`).toBeTruthy()
  const cards = (await listRes.json()) as Array<{
    title: string
    depends_on?: string[]
  }>
  const created = cards.find((c) => c.title === 'Needs Prereqs')
  expect(created, 'new card present in list').toBeTruthy()
  expect(created!.depends_on ?? []).toEqual([backlogCard.id])

  // Sanity: untouched seed cards are still around.
  expect(cards.find((c) => c.title === 'Running Prereq')?.title).toBe('Running Prereq')
  expect(cards.find((c) => c.title === 'Already Done')?.title).toBe('Already Done')
  void runningCard
  void doneCard
})
