import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * The model picker (ModelPicker.tsx) is a searchable combobox: clicking the
 * trigger opens a popup with a filter input over the model catalogue. This
 * proves the type-to-filter behaviour in the New Session modal — the same
 * component backs the session toolbar, project, card, and automation pickers.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

async function seedSession(
  request: APIRequestContext,
  authHeader: Record<string, string>,
): Promise<{ sessionId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-mp-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-mp-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'seed session', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  return { sessionId: session.id }
}

test('model picker filters the catalogue as you type', async ({ request, page, baseURL }) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)

  // The app-wide default model preselects in the modal. Set it up-front —
  // and restore it right after the label assert to keep the window where
  // other parallel specs could observe the changed global as small as
  // possible.
  const putDefault = (model: string) =>
    request.put('/api/settings/default-model', { headers: authHeader, data: { model } })
  expect((await putDefault('mock:echo')).ok()).toBeTruthy()
  const { sessionId } = await seedSession(request, authHeader)

  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto(`/sessions/${sessionId}`)
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })

  // Open the New Session modal via the tab strip's "+" button.
  await page.locator('.tab-new').click()

  // The model field is now a combobox trigger, not a native <select>.
  const trigger = page.getByTestId('new-session-model')
  await expect(trigger).toBeVisible({ timeout: 10_000 })
  // The configured default model arrives preselected.
  await expect(trigger).toContainText('Mock: echo')
  expect((await putDefault('')).ok()).toBeTruthy()

  await trigger.click()
  const search = page.getByTestId('new-session-model-search')
  await expect(search).toBeVisible()

  // Several mock models are listed before filtering.
  await expect(page.getByRole('option', { name: 'Mock: happy path' })).toBeVisible()
  await expect(page.getByRole('option', { name: 'Mock: echo' })).toBeVisible()

  // Typing narrows the list to matches only.
  await search.fill('happy')
  await expect(page.getByRole('option', { name: 'Mock: happy path' })).toBeVisible()
  await expect(page.getByRole('option', { name: 'Mock: echo' })).toHaveCount(0)
  await expect(page.getByRole('option', { name: 'Mock: crash' })).toHaveCount(0)

  // Selecting closes the popup and updates the trigger label.
  await page.getByRole('option', { name: 'Mock: happy path' }).click()
  await expect(search).toHaveCount(0)
  await expect(trigger).toContainText('Mock: happy path')

  // And the choice actually drives session creation.
  await page.getByPlaceholder('My session').fill('picker-test')
  await page.getByRole('button', { name: 'Create Session' }).click()

  await expect
    .poll(async () => {
      const res = await request.get('/api/sessions', { headers: authHeader })
      const { items } = (await res.json()) as {
        items: Array<{ name: string; model: string | null }>
      }
      return items.find((s) => s.name === 'picker-test')?.model ?? null
    })
    .toBe('mock:happy-path')
})

test('model picker groups by provider and flags unconfigured providers', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedSession(request, authHeader)

  // Deterministic "no usable auth" signal: serve the real catalogue but
  // mark the mock provider unconfigured — real providers' `configured`
  // depends on host credentials, which vary per machine.
  await page.route('**/api/models', async (route) => {
    const res = await route.fetch()
    const body = (await res.json()) as {
      providers: Array<{ id: string; configured?: boolean | null }>
    }
    for (const p of body.providers) if (p.id === 'mock') p.configured = false
    await route.fulfill({ response: res, json: body })
  })

  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto(`/sessions/${sessionId}`)
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })

  await page.locator('.tab-new').click()
  const trigger = page.getByTestId('new-session-model')
  await expect(trigger).toBeVisible({ timeout: 10_000 })
  await trigger.click()
  const search = page.getByTestId('new-session-model-search')
  await expect(search).toBeVisible()

  // The mock provider's models sit under a section heading carrying the
  // "not configured" tag, and its rows render subdued but stay selectable.
  const mockGroup = page.locator('.dropdown-group', { hasText: 'Mock' })
  await expect(mockGroup.locator('.dropdown-group-label')).toHaveText('Mock')
  await expect(mockGroup.locator('.dropdown-group-tag')).toHaveText('not configured')
  expect(await mockGroup.locator('.dropdown-item-dimmed').count()).toBeGreaterThan(0)

  // Warn-only footer points at Settings → Providers & Accounts.
  const footerLink = page.locator('.dropdown-footer a')
  await expect(footerLink).toHaveText('Settings → Providers & Accounts')
  await expect(footerLink).toHaveAttribute('href', '/settings/providers')

  // Search still works across groups: a match keeps its group heading, a
  // group with no matches disappears heading and all.
  await search.fill('happy')
  await expect(page.getByRole('option', { name: 'Mock: happy path' })).toBeVisible()
  await expect(mockGroup.locator('.dropdown-group-label')).toHaveText('Mock')
  await search.fill('no-such-model-xyz')
  await expect(page.locator('.dropdown-group-heading')).toHaveCount(0)
  await search.fill('')

  // "Not configured" is a hint, not a block — selection still works.
  await page.getByRole('option', { name: 'Mock: happy path' }).click()
  await expect(search).toHaveCount(0)
  await expect(trigger).toContainText('Mock: happy path')
})
