import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * A failed `/api/models` (or `/api/workflows`, `/api/projects/:id`) fetch
 * used to render as a silent empty picker / "No inter-worker communications
 * yet." — indistinguishable from "there is genuinely nothing here". These
 * specs prove the failure now surfaces an inline error with a Retry
 * affordance, and that Retry actually recovers.
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

async function createFolder(request: APIRequestContext, authHeader: Record<string, string>) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-resource-err-'))
  const res = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-resource-err-${Date.now()}`, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function createProject(request: APIRequestContext, authHeader: Record<string, string>) {
  const folderId = await createFolder(request, authHeader)
  const res = await request.post('/api/projects', {
    headers: authHeader,
    data: {
      name: `e2e-resource-err-${Date.now()}`,
      folder_id: folderId,
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(res.ok(), `create project failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto(route)
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })
}

test('failed /api/models fetch shows a Retry in the New Session modal, and Retry recovers', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  await createFolder(request, authHeader)

  const modelsPattern = '**/api/models'
  await page.route(modelsPattern, (route) => route.abort())

  await loadAt(page, token, '/')
  await page.locator('.tab-new').click()
  await expect(page.getByTestId('new-session-preset')).toBeVisible()

  const errorNotice = page.locator('.picker-load-error', { hasText: "Couldn't load models" })
  await expect(errorNotice).toBeVisible({ timeout: 10_000 })

  await page.unroute(modelsPattern)
  await errorNotice.getByRole('button', { name: 'Retry' }).click()

  await expect(errorNotice).toHaveCount(0, { timeout: 10_000 })
  await page.getByTestId('new-session-model').click()
  await expect(page.getByRole('option', { name: 'Mock: happy path' })).toBeVisible()
})

test('failed /api/workflows fetch shows a Retry in the card form', async ({ request, page }) => {
  const { token, authHeader } = await authenticate(request)
  const projectId = await createProject(request, authHeader)

  const workflowsPattern = '**/api/workflows'
  await page.route(workflowsPattern, (route) => route.abort())

  await loadAt(page, token, `/projects/${projectId}`)
  await page.getByRole('button', { name: 'Add Card' }).click()
  await expect(page.locator('#card-title')).toBeVisible({ timeout: 10_000 })

  const errorNotice = page.locator('.picker-load-error', { hasText: "Couldn't load workflows" })
  await expect(errorNotice).toBeVisible({ timeout: 10_000 })

  await page.unroute(workflowsPattern)
  await errorNotice.getByRole('button', { name: 'Retry' }).click()
  await expect(errorNotice).toHaveCount(0, { timeout: 10_000 })
})

test('failed worker-comms project fetch shows an error notice with Retry', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const projectId = await createProject(request, authHeader)

  const projectPattern = `**/api/projects/${projectId}`
  await page.route(projectPattern, (route) => route.abort())

  await loadAt(page, token, `/projects/${projectId}`)
  await page.getByRole('button', { name: 'Worker communications' }).click()

  const errorNotice = page.locator('.worker-comms-empty', {
    hasText: 'Failed to load worker communications.',
  })
  await expect(errorNotice).toBeVisible({ timeout: 10_000 })
  await expect(page.locator('.worker-comms-empty', { hasText: 'No inter-worker' })).toHaveCount(0)

  await page.unroute(projectPattern)
  await errorNotice.getByRole('button', { name: 'Retry' }).click()

  await expect(errorNotice).toHaveCount(0, { timeout: 10_000 })
})
