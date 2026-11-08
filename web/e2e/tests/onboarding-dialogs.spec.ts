import { test, expect, type APIRequestContext, type Page } from '../harness'
import { existsSync, mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * The create dialogs a first-time user meets — New Session, New Project and
 * New Card — need no decisions beyond the obvious ones:
 *   1. New Session creates with no name typed and the folder already picked,
 *      and says why Create is disabled when there is no folder at all;
 *   2. New Project creates straight away with the Task workflow preselected;
 *   3. New Card's "Add and start" runs the board's cost confirmation, then
 *      the card leaves Backlog and a worker picks it up.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

let cachedAuth: { token: string; auth: Record<string, string> } | null = null

/** Authenticate once per spec file — the per-IP login limiter sees the
 *  whole suite as one client. */
async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  if (cachedAuth) return cachedAuth
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  cachedAuth = { token, auth: { Authorization: `Bearer ${token}` } }
  return cachedAuth
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

async function seedFolder(
  request: APIRequestContext,
  auth: Record<string, string>,
  tag: string,
): Promise<string> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-${tag}-`))
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-${tag}-${Date.now()}`, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

test('New Session creates with no name typed and a preselected folder', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  await seedFolder(request, auth, 'onboard-session')

  const listSessions = async () =>
    (
      (await (await request.get('/api/sessions?limit=200', { headers: auth })).json()) as {
        items: { id: string; name: string; folder_id: string }[]
      }
    ).items
  const before = new Set((await listSessions()).map((s) => s.id))

  await loadAt(page, token, '/')
  await page.locator('.tab-new').click()
  await expect(page.locator('#new-session-name')).toBeVisible({ timeout: 10_000 })

  // Rarely-needed settings start hidden behind "More options".
  await expect(page.getByTestId('new-session-preset')).toHaveCount(0)
  await expect(page.getByTestId('new-session-more-options')).toBeVisible()

  // A folder is already chosen; nothing blocks Create.
  const folderSelect = page.locator('#new-session-folder')
  const folderId = await folderSelect.inputValue()
  expect(folderId).not.toBe('')

  await page.getByTestId('new-session-model').click()
  await page.getByTestId('new-session-model-search').fill('happy')
  await page.getByRole('option', { name: 'Mock: happy path' }).click()
  await expect(page.getByTestId('new-session-disabled-reason')).toHaveCount(0)
  await page.getByTestId('new-session-create').click()

  await expect(page.locator('.chat-container')).toBeVisible({ timeout: 10_000 })
  let created: { name: string; folder_id: string } | undefined
  await expect
    .poll(async () => {
      created = (await listSessions()).find((s) => !before.has(s.id))
      return created?.name ?? null
    })
    .toBe('New session')
  expect(created!.folder_id).toBe(folderId)
})

test('New Session says why Create is disabled when there is no folder', async ({
  request,
  page,
}) => {
  const { token } = await authenticate(request)
  // Other specs share this server's folders, so fake an empty list.
  await page.route('**/api/folders', (route) =>
    route.request().method() === 'GET'
      ? route.fulfill({ status: 200, contentType: 'application/json', body: '[]' })
      : route.continue(),
  )
  // Point the ready-made home folder at a throwaway dir, never the real $HOME.
  const suggested = path.join(mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-home-')), 'PeckBoard')
  await page.route('**/api/folders/home', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ home: path.dirname(suggested), suggested }),
    }),
  )
  await loadAt(page, token, '/')
  await page.locator('.tab-new').click()
  await expect(page.getByTestId('new-session-no-folders')).toBeVisible({ timeout: 10_000 })
  await expect(page.getByTestId('new-session-create')).toBeDisabled()
  await expect(page.getByTestId('new-session-disabled-reason')).toHaveText('Add a folder first')
  await expect(page.getByTestId('new-session-new-folder')).toBeVisible()

  // The one-click ready-made folder creates it and selects it.
  const homeButton = page.getByTestId('new-session-home-folder')
  await expect(homeButton).toContainText(suggested)
  await homeButton.click()
  await expect(page.locator('#new-session-folder option:checked')).toContainText(suggested)
  await expect(page.getByTestId('new-session-disabled-reason')).toHaveCount(0)
  expect(existsSync(suggested)).toBe(true)
})

test('New Project creates with the Task workflow preselected', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const folderId = await seedFolder(request, auth, 'onboard-project')

  await loadAt(page, token, '/projects')
  await page.getByRole('button', { name: '+ New project' }).click()
  await expect(page.getByRole('heading', { name: 'New Project' })).toBeVisible()

  const name = `onboard task ${Date.now()}`
  await page.locator('#new-project-name').fill(name)
  await page.locator('#new-project-folder').selectOption(folderId)
  await expect(page.locator('.workflow-select-trigger')).toContainText('Task')
  await page.getByRole('button', { name: 'Create Project' }).click()
  await expect(page.getByRole('heading', { name: 'New Project' })).toHaveCount(0, {
    timeout: 10_000,
  })

  const res = await request.get('/api/projects', { headers: auth })
  expect(res.ok()).toBeTruthy()
  const body = (await res.json()) as
    | { name: string; workflow: string }[]
    | { items: { name: string; workflow: string }[] }
  const projects = Array.isArray(body) ? body : body.items
  expect(projects.find((p) => p.name === name)?.workflow).toBe('task')
})

test('"Add and start" confirms the cost, then a worker picks the card up', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const folderId = await seedFolder(request, auth, 'onboard-start')
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: `onboard start ${Date.now()}`,
      folder_id: folderId,
      worker_count: 1,
      workflow: 'task',
      model: 'mock:happy-path',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  await loadAt(page, token, `/projects/${project.id}`)
  // The empty board says cards wait in Backlog until started.
  await expect(page.getByTestId('kanban-empty')).toContainText('wait in Backlog', {
    timeout: 10_000,
  })
  await page.getByTestId('kanban-empty-add').click()

  const modal = page.locator('.modal').filter({ hasText: 'New Card' })
  await expect(modal).toBeVisible()
  await modal.locator('#card-title').fill('Start me now')
  await modal.getByTestId('card-form-add-start').click()
  await expect(modal).toBeHidden({ timeout: 10_000 })

  const dialog = page.getByTestId('backlog-start-confirm')
  await expect(dialog).toBeVisible()
  await expect(dialog).toContainText('costs money')
  await dialog.getByRole('button', { name: 'Start work' }).click()

  type CardRow = {
    title: string
    step: string
    worker_session_id: string | null
    last_worker_session_id: string | null
  }
  const ours = async () => {
    const res = await request.get(`/api/projects/${project.id}/cards`, { headers: auth })
    const cards = (await res.json()) as CardRow[]
    return cards.find((c) => c.title === 'Start me now')
  }
  await expect.poll(async () => (await ours())?.step, { timeout: 10_000 }).not.toBe('backlog')
  await expect
    .poll(
      async () => {
        const c = await ours()
        return c?.worker_session_id ?? c?.last_worker_session_id ?? null
      },
      { timeout: 30_000, message: 'a worker should pick the started card up' },
    )
    .not.toBeNull()
})
