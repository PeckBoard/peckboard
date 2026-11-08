import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Onboarding on a fresh install:
 *
 * - The Getting Started checklist on Sessions ticks its steps from real data
 *   (here: a folder created through the API), hides on dismiss, stays hidden
 *   across reloads, and comes back from the user menu.
 * - Top-level pages explain themselves when empty (Projects, Folders).
 * - The remote-computers page is called Machines, so "agents" only ever
 *   means the AI.
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

/** Make the UI see no folders (the shard's server may already hold some). */
async function hideFolders(page: Page) {
  await page.route('**/api/folders', async (route) => {
    if (route.request().method() !== 'GET') return route.continue()
    await route.fulfill({ status: 200, contentType: 'application/json', body: '[]' })
  })
}

test('getting started ticks a folder from real data, dismisses, and comes back', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  await hideFolders(page)
  await loadAt(page, token, '/sessions')

  const checklist = page.getByTestId('getting-started')
  await expect(checklist).toBeVisible({ timeout: 10_000 })
  await expect(checklist.getByRole('heading', { name: 'Getting started' })).toBeVisible()
  const folderStep = checklist.getByTestId('getting-started-folder')
  await expect(folderStep).toHaveAttribute('data-done', 'false')
  await expect(folderStep.getByTestId('getting-started-folder-action')).toBeVisible()

  // A folder created outside the checklist (API, another tab, an agent)
  // ticks the step: ticks come from data, never from clicks.
  await page.unroute('**/api/folders')
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-getting-started-'))
  const created = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-getting-started-${Date.now()}`, path: folderPath },
  })
  expect(created.ok(), `create folder failed: ${await created.text()}`).toBeTruthy()
  await page.reload()
  await expect(folderStep).toHaveAttribute('data-done', 'true', { timeout: 10_000 })
  await expect(folderStep.getByTestId('getting-started-folder-action')).toHaveCount(0)

  // Dismiss: gone, and still gone after a reload.
  await checklist.getByTestId('getting-started-dismiss').click()
  await expect(checklist).toBeHidden()
  await page.reload()
  await expect(page.getByRole('heading', { name: 'Sessions', exact: true })).toBeVisible({
    timeout: 10_000,
  })
  await expect(page.getByTestId('getting-started')).toHaveCount(0)

  // The user menu brings it back, from any page.
  await page.getByRole('button', { name: 'Projects', exact: true }).click()
  await expect(page).toHaveURL(/\/projects/)
  await page.getByRole('button', { name: 'User menu' }).click()
  await page.getByTestId('user-menu-getting-started').click()
  await expect(page).not.toHaveURL(/\/projects/)
  await expect(page.getByTestId('getting-started')).toBeVisible()

  // The shortcuts dialog is reachable without knowing the `?` key.
  await page.getByRole('button', { name: 'User menu' }).click()
  await page.getByTestId('user-menu-shortcuts').click()
  await expect(page.getByRole('heading', { name: 'Keyboard shortcuts' })).toBeVisible()
})

test('empty pages say what they are for, and remote computers are Machines', async ({
  request,
  page,
}) => {
  const { token } = await authenticate(request)
  await hideFolders(page)
  await page.route('**/api/projects', async (route) => {
    if (route.request().method() !== 'GET') return route.continue()
    await route.fulfill({ status: 200, contentType: 'application/json', body: '[]' })
  })
  await loadAt(page, token, '/projects')

  const projectsEmpty = page.getByTestId('projects-empty')
  await expect(projectsEmpty).toBeVisible({ timeout: 10_000 })
  await expect(projectsEmpty).toContainText('No projects yet')
  await expect(projectsEmpty).toContainText('a list of tasks')
  await expect(
    projectsEmpty.getByRole('button', { name: 'Create your first project' }),
  ).toBeVisible()
  await projectsEmpty.getByTestId('help-tip-project').click()
  await expect(page.getByTestId('help-tip-popover')).toContainText('A board of tasks')

  await page.getByRole('button', { name: 'Folders', exact: true }).click()
  const foldersEmpty = page.getByTestId('folders-empty')
  await expect(foldersEmpty).toBeVisible()
  await expect(foldersEmpty).toContainText('A folder is a place on this computer')
  await expect(foldersEmpty.getByRole('button', { name: 'Add your first folder' })).toBeVisible()

  // Remote computers: "Machines" in the rail and on the page.
  const rail = page.getByTestId('rail-agents')
  await expect(rail).toHaveAttribute('aria-label', 'Machines')
  await rail.click()
  const machines = page.getByTestId('agents-view')
  await expect(machines.getByRole('heading', { name: 'Machines' })).toBeVisible()
  await expect(page.getByTestId('agents-enroll')).toHaveText('+ Add machine')
})
