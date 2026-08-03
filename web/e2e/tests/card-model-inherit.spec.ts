import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * A card's model is an inherit chain: card → step → project → app default.
 * A card with `model = null` deliberately inherits, and editing any other
 * field must not pin a model onto it — the form used to materialize the
 * app-wide default into its value and PATCH it back on every save, silently
 * changing which model (and whose spend) future workers used.
 *
 * This covers, through the real UI:
 *   - the picker shows the inherited model as a "Default (…)" row rather
 *     than a hard pin,
 *   - a title-only edit leaves the stored `model` null,
 *   - explicitly picking a model still pins it,
 *   - picking the "Default (…)" row again un-pins it back to null.
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

test('editing a card without touching the model keeps it inheriting', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-card-model-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-card-model-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  // The project pins a model, so an inheriting card resolves to it — and
  // worker_count=0 keeps the orchestrator from moving the card mid-test.
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: 'card model inherit',
      folder_id: folder.id,
      worker_count: 0,
      workflow: 'task',
      model: 'mock:happy-path',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  // Seeded with no model at all: this card inherits.
  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: auth,
    data: { title: 'Inherits Model', description: 'seed', step: 'backlog', priority: 2 },
  })
  expect(cardRes.ok(), `create card failed: ${await cardRes.text()}`).toBeTruthy()
  const card = (await cardRes.json()) as { id: string; model: string | null }
  expect(card.model, 'seeded card inherits').toBeNull()

  const storedModel = async () => {
    const res = await request.get(`/api/projects/${project.id}/cards`, { headers: auth })
    expect(res.ok(), `list cards failed: ${await res.text()}`).toBeTruthy()
    const cards = (await res.json()) as Array<{ id: string; model: string | null }>
    return cards.find((c) => c.id === card.id)?.model ?? null
  }

  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto(`/projects/${project.id}`)

  const openEditor = async (p: Page) => {
    // The quick-action row only renders on an expanded card.
    const expand = p.getByTestId('card-expand-toggle').first()
    await expect(expand).toBeVisible({ timeout: 15_000 })
    if ((await expand.getAttribute('aria-expanded')) !== 'true') await expand.click()
    await p.getByTestId('card-quick-edit').first().click()
    const modal = p.locator('.modal').filter({ hasText: 'Edit Card' })
    await expect(modal).toBeVisible({ timeout: 10_000 })
    return modal
  }

  let modal = await openEditor(page)

  // The picker names the model this card actually runs on instead of
  // pre-filling a pin the user never chose.
  const trigger = page.getByTestId('card-model')
  await expect(trigger).toContainText('Default (Mock: happy path)')

  // Edit only the title.
  await modal.locator('#card-title').fill('Inherits Model (renamed)')
  await modal.getByRole('button', { name: 'Save' }).click()
  await expect(modal).toBeHidden({ timeout: 10_000 })

  await expect.poll(storedModel, { timeout: 10_000 }).toBeNull()

  // Explicitly picking a model still pins it.
  modal = await openEditor(page)
  await trigger.click()
  await page.getByRole('option', { name: 'Mock: echo' }).click()
  await expect(trigger).toContainText('Mock: echo')
  await modal.getByRole('button', { name: 'Save' }).click()
  await expect(modal).toBeHidden({ timeout: 10_000 })
  await expect.poll(storedModel, { timeout: 10_000 }).toBe('mock:echo')

  // And the default row un-pins it back to inheriting.
  modal = await openEditor(page)
  await expect(trigger).toContainText('Mock: echo')
  await trigger.click()
  await page.getByTestId('card-model-option-default').click()
  await expect(trigger).toContainText('Default (Mock: happy path)')
  await modal.getByRole('button', { name: 'Save' }).click()
  await expect(modal).toBeHidden({ timeout: 10_000 })
  await expect.poll(storedModel, { timeout: 10_000 }).toBeNull()
})
