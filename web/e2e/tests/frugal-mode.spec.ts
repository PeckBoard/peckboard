import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e for cost-aware model auto-switch ("Frugal Mode"):
 *
 * 1. The Add Card modal shows the auto-switch toggle, defaulted ON (cards
 *    spawn workers, which default ON), and the created card persists that
 *    choice — unchecking it stores `model_autoswitch=false`.
 * 2. Settings → System Prompts: creating a named prompt lists it and
 *    persists it via `GET /api/system-prompts`.
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

test('Add Card modal defaults auto-switch ON and persists the choice', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-frugal-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-frugal-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), await folderRes.text()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  // worker_count=0 keeps cards parked in backlog so nothing races.
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: { name: 'frugal', folder_id: folder.id, worker_count: 0, workflow: 'task' },
  })
  expect(projectRes.ok(), await projectRes.text()).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  await loadAt(page, token, `/projects/${project.id}`)

  // Card 1: leave the toggle untouched — it must default ON.
  await page.getByRole('button', { name: 'Add Card' }).click()
  let modal = page.locator('.modal').filter({ hasText: 'New Card' })
  await expect(modal).toBeVisible({ timeout: 10_000 })
  await expect(modal.getByTestId('card-autoswitch')).toBeChecked()
  await modal.locator('input.form-input').first().fill('Autoswitch default card')
  await modal.getByRole('button', { name: 'Create Card' }).click()
  await expect(modal).toBeHidden({ timeout: 10_000 })

  // Card 2: uncheck the toggle before creating.
  await page.getByRole('button', { name: 'Add Card' }).click()
  modal = page.locator('.modal').filter({ hasText: 'New Card' })
  await expect(modal).toBeVisible({ timeout: 10_000 })
  await modal.getByTestId('card-autoswitch').uncheck()
  await modal.locator('input.form-input').first().fill('Autoswitch off card')
  await modal.getByRole('button', { name: 'Create Card' }).click()
  await expect(modal).toBeHidden({ timeout: 10_000 })

  const listRes = await request.get(`/api/projects/${project.id}/cards`, { headers: auth })
  expect(listRes.ok(), await listRes.text()).toBeTruthy()
  const cards = (await listRes.json()) as Array<{
    title: string
    model_autoswitch: boolean | null
  }>
  const onCard = cards.find((c) => c.title === 'Autoswitch default card')
  const offCard = cards.find((c) => c.title === 'Autoswitch off card')
  expect(onCard, 'default card present').toBeTruthy()
  expect(offCard, 'off card present').toBeTruthy()
  expect(onCard!.model_autoswitch).toBe(true)
  expect(offCard!.model_autoswitch).toBe(false)
})

test('Settings System Prompts page creates and lists a prompt', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)

  await loadAt(page, token, '/settings')
  await expect(page.getByTestId('settings-page')).toBeVisible({ timeout: 10_000 })

  await page.getByTestId('settings-nav-prompts').click()
  await expect(page.getByTestId('system-prompts-section')).toBeVisible({ timeout: 10_000 })

  const name = `e2e-prompt-${Date.now()}`
  await page.getByTestId('system-prompt-new').click()
  await page.getByTestId('system-prompt-name-input').fill(name)
  await page.getByTestId('system-prompt-body-input').fill('You are an e2e test prompt.')
  await page.getByTestId('system-prompt-save').click()

  // The new prompt shows up in the list.
  await expect(page.getByTestId(`system-prompt-${name}`)).toBeVisible({ timeout: 10_000 })

  // ...and is persisted server-side.
  const listRes = await request.get('/api/system-prompts', { headers: auth })
  expect(listRes.ok(), await listRes.text()).toBeTruthy()
  const { prompts } = (await listRes.json()) as {
    prompts: Array<{ name: string; body: string }>
  }
  const saved = prompts.find((p) => p.name === name)
  expect(saved, 'prompt persisted').toBeTruthy()
  expect(saved!.body).toBe('You are an e2e test prompt.')
})
