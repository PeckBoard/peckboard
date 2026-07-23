import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Regression coverage for creating a project through the New Project
 * modal. 0.0.99 dropped `name` from the create payload, so the server
 * rejected every UI create with a plain-text 422 the modal could only
 * render as the generic "Failed to create project" banner — while the
 * HTTP API path (which every other spec uses) kept working. Drive the
 * real modal so the payload the UI actually sends stays honest.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; authHeader: { Authorization: string } }> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

test('New Project modal creates a project end-to-end', async ({ request, page }) => {
  const { token, authHeader } = await authenticate(request)

  // A registered folder so the modal's folder dropdown has an entry to
  // default to.
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-project-create-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-project-create', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()

  await loadAppAt(page, token, '/projects')

  await page.getByRole('button', { name: '+ New project' }).click()
  const modalHeading = page.getByRole('heading', { name: 'New Project' })
  await expect(modalHeading).toBeVisible()

  await page.getByPlaceholder('My project').fill('e2e ui project')

  // Submit stays disabled until a workflow is picked.
  const submit = page.getByRole('button', { name: 'Create Project' })
  await expect(submit).toBeDisabled()
  await page.locator('.workflow-select-trigger').click()
  await page.getByRole('menuitem', { name: /Fast Develop Software/ }).click()
  await expect(submit).toBeEnabled()
  await submit.click()

  // Success closes the modal; the regression instead kept it open with
  // the generic "Failed to create project" error.
  await expect(modalHeading).toBeHidden({ timeout: 10_000 })
  await expect(page.locator('.form-error')).toHaveCount(0)

  // The project really exists server-side with the typed name.
  const listRes = await request.get('/api/projects', { headers: authHeader })
  expect(listRes.ok(), `list projects failed: ${await listRes.text()}`).toBeTruthy()
  const projects = (await listRes.json()) as Array<{ name: string; workflow: string }>
  const created = projects.find((p) => p.name === 'e2e ui project')
  expect(created, 'created project present in /api/projects').toBeTruthy()
  expect(created?.workflow).toBe('fast-develop-software')
})
