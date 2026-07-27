import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * First-run empty / loading states on the projects surface:
 *
 * - The New Project modal used to dead-end when no folder was
 *   registered: plain text telling you to go elsewhere, and a disabled
 *   Create button with no stated reason.
 * - `ProjectList` ignored `projectsLoaded`, so "No projects yet"
 *   flashed on every cold load.
 * - A card-less board repeated an italic "No cards in <step>" note in
 *   every column instead of offering one next step.
 * - A long project list had no filter.
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

async function seedFolder(request: APIRequestContext, auth: Record<string, string>, tag: string) {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-${tag}-`))
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-${tag}-${Date.now()}`, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as { id: string }
}

test('no-folder first run creates a folder and a project without leaving the modal', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)

  // Simulate a fresh install: the UI sees zero folders. POSTs still hit
  // the real server, so the folder the user adds is genuine.
  await page.route('**/api/folders', async (route) => {
    if (route.request().method() !== 'GET') return route.continue()
    await route.fulfill({ status: 200, contentType: 'application/json', body: '[]' })
  })

  await loadAt(page, token, '/projects')
  await page.getByRole('button', { name: '+ New project' }).click()
  const modalHeading = page.getByRole('heading', { name: 'New Project' })
  await expect(modalHeading).toBeVisible()

  // The disabled submit names the field at fault.
  await page.getByPlaceholder('My project').fill('e2e first run project')
  const submit = page.getByRole('button', { name: 'Create Project' })
  await expect(submit).toBeDisabled()
  await expect(page.getByTestId('new-project-disabled-reason')).toHaveText('Add a folder first')

  // The folder manager opens stacked on top — the form is not lost.
  await page.getByTestId('new-project-add-folder').click()
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-first-run-'))
  const folderName = `e2e-first-run-${Date.now()}`
  await page.getByPlaceholder('Name (e.g. My Workspace)').fill(folderName)
  await page.getByPlaceholder('Path (e.g. /Users/me/projects)').fill(folderPath)
  await page.getByRole('button', { name: 'Add Folder' }).click()
  await expect(page.locator('.folder-row', { hasText: folderName })).toBeVisible({
    timeout: 10_000,
  })
  await page.getByTestId('new-project-folders-done').click()

  // Back in the New Project form with the typed name intact and the
  // freshly created folder selected.
  await expect(modalHeading).toBeVisible()
  await expect(page.getByPlaceholder('My project')).toHaveValue('e2e first run project')
  await expect(page.locator('#new-project-folder')).toBeVisible()

  await page.locator('.workflow-select-trigger').click()
  await page.getByRole('menuitem', { name: /Fast Develop Software/ }).click()
  await expect(page.getByTestId('new-project-disabled-reason')).toHaveCount(0)
  await expect(submit).toBeEnabled()
  await submit.click()

  await expect(modalHeading).toBeHidden({ timeout: 10_000 })
  const listRes = await request.get('/api/projects', { headers: auth })
  const projects = (await listRes.json()) as Array<{ name: string }>
  expect(projects.some((p) => p.name === 'e2e first run project')).toBeTruthy()
})

test('cold load shows a loading state, never a "No projects yet" flash', async ({
  request,
  page,
}) => {
  const { token } = await authenticate(request)

  let release = () => {}
  const gate = new Promise<void>((resolve) => {
    release = resolve
  })
  await page.route('**/api/projects', async (route) => {
    if (route.request().method() !== 'GET') return route.continue()
    await gate
    await route.continue()
  })

  await loadAt(page, token, '/projects')

  await expect(page.getByTestId('projects-loading')).toBeVisible({ timeout: 10_000 })
  await expect(page.getByText('No projects yet')).toHaveCount(0)

  release()
  await expect(page.getByTestId('projects-loading')).toHaveCount(0, { timeout: 10_000 })
})

test('a card-less board offers one empty state whose CTA opens the card form', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const folder = await seedFolder(request, auth, 'board-empty')
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: `board empty ${Date.now()}`,
      folder_id: folder.id,
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  await loadAt(page, token, `/projects/${project.id}`)

  const empty = page.getByTestId('kanban-empty')
  await expect(empty).toBeVisible({ timeout: 10_000 })
  await expect(empty).toContainText('No cards yet')
  // One board-level state replaces the per-column placeholders.
  await expect(page.locator('.kanban-cards-empty')).toHaveCount(0)

  await page.getByTestId('kanban-empty-add').click()
  await expect(page.getByRole('heading', { name: /New Card/i })).toBeVisible({ timeout: 5_000 })
})

test('the projects filter narrows the list by name', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const folder = await seedFolder(request, auth, 'filter')
  const stamp = Date.now()
  const alpha = `zz-filter-alpha-${stamp}`
  const beta = `zz-filter-beta-${stamp}`
  for (const name of [alpha, beta]) {
    const res = await request.post('/api/projects', {
      headers: auth,
      data: { name, folder_id: folder.id, worker_count: 0, workflow: 'task' },
    })
    expect(res.ok(), `create project failed: ${await res.text()}`).toBeTruthy()
  }

  await loadAt(page, token, '/projects')
  await expect(page.getByText(alpha, { exact: true })).toBeVisible({ timeout: 10_000 })

  await page.getByTestId('project-filter').fill(`filter-alpha-${stamp}`)
  await expect(page.getByText(alpha, { exact: true })).toBeVisible()
  await expect(page.getByText(beta, { exact: true })).toHaveCount(0)

  // A filter that matches nothing gets its own state, not "No projects yet".
  await page.getByTestId('project-filter').fill('no-such-project-zzzz')
  await expect(page.getByTestId('projects-filter-empty')).toBeVisible()
  await expect(page.getByText('No projects yet')).toHaveCount(0)
})
