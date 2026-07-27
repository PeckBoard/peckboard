import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Destructive actions that used to fire on a single click now go through
 * the shared `ConfirmDialog`. One test per surface, each asserting the
 * same contract: the dialog appears, Cancel is a no-op on the server, and
 * Confirm performs the action.
 *
 * Covered here: "Cancel as Won't Do" on a kanban card, folder delete,
 * user delete, "Upgrade & restart", and the Claude permission Bypass
 * toggle (plus the standing badge it puts on the settings hub).
 *
 * Projects use `worker_count: 0` so the orchestrator can't move a card
 * itself and race the assertions.
 */

const ADMIN_USER = 'e2e-user'
const ADMIN_PASS = 'e2e-password-1234'

let cachedAuth: { token: string; auth: Record<string, string> } | null = null

/** Authenticate as the bootstrap admin once per spec file — the per-IP
 *  login limiter sees the whole suite as one client. */
async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  if (cachedAuth) return cachedAuth
  const res = await request.post('/api/auth/login', {
    data: { username: ADMIN_USER, password: ADMIN_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  cachedAuth = { token, auth: { Authorization: `Bearer ${token}` } }
  return cachedAuth
}

/** Plant a token in localStorage and load the SPA at the given route. */
async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

async function createFolder(
  request: APIRequestContext,
  auth: Record<string, string>,
  slug: string,
): Promise<{ id: string; name: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-${slug}-`))
  const name = `e2e-${slug}-${Date.now()}`
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  const folder = (await res.json()) as { id: string }
  return { id: folder.id, name }
}

async function folderExists(
  request: APIRequestContext,
  auth: Record<string, string>,
  id: string,
): Promise<boolean> {
  const res = await request.get('/api/folders', { headers: auth })
  expect(res.ok(), `list folders failed: ${await res.text()}`).toBeTruthy()
  const folders = (await res.json()) as { id: string }[]
  return folders.some((f) => f.id === id)
}

async function stepOf(
  request: APIRequestContext,
  auth: Record<string, string>,
  projectId: string,
  title: string,
): Promise<string | undefined> {
  const res = await request.get(`/api/projects/${projectId}/cards`, { headers: auth })
  expect(res.ok(), `list cards failed: ${await res.text()}`).toBeTruthy()
  const cards = (await res.json()) as { title: string; step: string }[]
  return cards.find((c) => c.title === title)?.step
}

test('"Cancel as Won\'t Do" is confirmed first, and cancelling leaves the card alone', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const folder = await createFolder(request, auth, 'wontdo')
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: `wont-do confirm ${Date.now()}`,
      folder_id: folder.id,
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const projectId = ((await projectRes.json()) as { id: string }).id

  const cardRes = await request.post(`/api/projects/${projectId}/cards`, {
    headers: auth,
    data: { title: 'Doomed', description: '', step: 'backlog', priority: 0 },
  })
  expect(cardRes.ok(), `seed card failed: ${await cardRes.text()}`).toBeTruthy()

  await loadAt(page, token, `/projects/${projectId}`)
  const card = page.locator('.kanban-card', { hasText: 'Doomed' })
  await expect(card).toBeVisible({ timeout: 10_000 })

  const dialog = page.locator('[data-testid="card-wont-do-confirm"]')

  // Menu → the action asks first and names the card.
  await card.locator('.kanban-card-menu-btn').click()
  await page.locator('[data-testid="card-menu-wont-do"]').click()
  await expect(dialog).toBeVisible()
  await expect(dialog).toContainText('Doomed')
  await expect(dialog).toContainText('terminal step')

  // Cancel → nothing happened, in the UI or on the server.
  await dialog.locator('[data-testid="confirm-dialog-cancel"]').click()
  await expect(dialog).toHaveCount(0)
  expect(await stepOf(request, auth, projectId, 'Doomed')).toBe('backlog')

  // Confirm → the card lands in the terminal step.
  await page.locator('.kanban-card', { hasText: 'Doomed' }).locator('.kanban-card-menu-btn').click()
  await page.locator('[data-testid="card-menu-wont-do"]').click()
  await expect(dialog).toBeVisible()
  await dialog.locator('[data-testid="confirm-dialog-confirm"]').click()
  await expect(dialog).toHaveCount(0)
  await expect.poll(() => stepOf(request, auth, projectId, 'Doomed')).toBe('wont_do')
})

test('deleting a folder is confirmed first, including an empty one', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const folder = await createFolder(request, auth, 'folderdel')

  await loadAt(page, token, '/folders')
  const deleteBtn = page.locator(`[data-testid="folder-delete-${folder.name}"]`)
  await expect(deleteBtn).toBeVisible({ timeout: 10_000 })

  const dialog = page.locator('[data-testid="folder-delete-confirm"]')

  // Cancel → the folder is still registered (this is the case that used
  // to vanish on a single click, because an empty folder never hit 409).
  await deleteBtn.click()
  await expect(dialog).toBeVisible()
  await expect(dialog).toContainText(folder.name)
  await dialog.locator('[data-testid="confirm-dialog-cancel"]').click()
  await expect(dialog).toHaveCount(0)
  expect(await folderExists(request, auth, folder.id)).toBe(true)

  // Confirm → gone.
  await deleteBtn.click()
  await expect(dialog).toBeVisible()
  await dialog.locator('[data-testid="confirm-dialog-confirm"]').click()
  await expect(dialog).toHaveCount(0)
  await expect(deleteBtn).toHaveCount(0)
  await expect.poll(() => folderExists(request, auth, folder.id)).toBe(false)
})

test('deleting a user is confirmed first and names the user', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const username = `confirm-del-${Date.now()}`
  const createRes = await request.post('/api/users', {
    headers: auth,
    data: { username, password: 'throwaway-password-1234', role: 'user' },
  })
  expect(createRes.ok(), `create user failed: ${await createRes.text()}`).toBeTruthy()

  const userExists = async () => {
    const res = await request.get('/api/users', { headers: auth })
    expect(res.ok(), `list users failed: ${await res.text()}`).toBeTruthy()
    const users = (await res.json()) as { username: string }[]
    return users.some((u) => u.username === username)
  }

  await loadAt(page, token, '/users')
  const deleteBtn = page.locator(`[data-testid="user-delete-${username}"]`)
  await expect(deleteBtn).toBeVisible({ timeout: 10_000 })

  const dialog = page.locator('[data-testid="user-delete-confirm"]')

  await deleteBtn.click()
  await expect(dialog).toBeVisible()
  await expect(dialog).toContainText(username)
  await expect(dialog).toContainText('can no longer sign in')
  await dialog.locator('[data-testid="confirm-dialog-cancel"]').click()
  await expect(dialog).toHaveCount(0)
  expect(await userExists()).toBe(true)

  await deleteBtn.click()
  await expect(dialog).toBeVisible()
  await dialog.locator('[data-testid="confirm-dialog-confirm"]').click()
  await expect(dialog).toHaveCount(0)
  await expect(deleteBtn).toHaveCount(0)
  await expect.poll(() => userExists()).toBe(false)
})

test('"Upgrade & restart" only POSTs the apply after the confirmation', async ({
  request,
  page,
}) => {
  const { token } = await authenticate(request)

  // The test host has no newer release, so the check endpoint is stubbed.
  // After apply, the stub goes unreachable — that mirrors the real restart
  // and keeps the component's poll from reloading the page mid-assertion.
  let applied = false
  let applyCalls = 0
  await page.route('**/api/update/check', async (route) => {
    if (applied) {
      await route.fulfill({ status: 503, body: '' })
      return
    }
    await route.fulfill({
      json: {
        current_version: '0.0.1',
        latest_version: '9.9.9',
        update_available: true,
        supported: true,
        asset: 'peckboard-linux-x86_64',
        notes: null,
        html_url: null,
      },
    })
  })
  await page.route('**/api/update/apply', async (route) => {
    applyCalls += 1
    applied = true
    await route.fulfill({ json: { ok: true } })
  })

  await loadAt(page, token, '/settings')
  await page.locator('[data-testid="settings-nav-server"]').click()
  const applyBtn = page.locator('[data-testid="update-apply"]')
  await expect(applyBtn).toBeVisible({ timeout: 10_000 })

  const dialog = page.locator('[data-testid="update-apply-confirm"]')

  // Cancel → no request left the browser.
  await applyBtn.click()
  await expect(dialog).toBeVisible()
  await expect(dialog).toContainText('9.9.9')
  await expect(dialog).toContainText('restarts the server')
  await dialog.locator('[data-testid="confirm-dialog-cancel"]').click()
  await expect(dialog).toHaveCount(0)
  expect(applyCalls).toBe(0)

  // Confirm → exactly one apply, and the section switches to "restarting".
  await applyBtn.click()
  await expect(dialog).toBeVisible()
  await dialog.locator('[data-testid="confirm-dialog-confirm"]').click()
  await expect(page.locator('[data-testid="update-restarting"]')).toBeVisible()
  expect(applyCalls).toBe(1)
})

test('bypassing Claude tool permissions is confirmed and then badged on the settings hub', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)

  const bypassState = async () => {
    const res = await request.get('/api/settings/claude-permissions', { headers: auth })
    expect(res.ok(), `read permission mode failed: ${await res.text()}`).toBeTruthy()
    return ((await res.json()) as { bypass: boolean }).bypass
  }

  try {
    await loadAt(page, token, '/settings')
    await page.locator('[data-testid="settings-nav-server"]').click()
    const bypassBtn = page.locator('[data-testid="claude-permissions-bypass"]')
    await expect(bypassBtn).toBeVisible({ timeout: 10_000 })

    const dialog = page.locator('[data-testid="claude-bypass-confirm"]')

    // Cancel → the host stays enforced.
    await bypassBtn.click()
    await expect(dialog).toBeVisible()
    await expect(dialog).toContainText('dangerously-skip-permissions')
    await dialog.locator('[data-testid="confirm-dialog-cancel"]').click()
    await expect(dialog).toHaveCount(0)
    expect(await bypassState()).toBe(false)

    // Confirm → the mode flips and the hub carries a standing warning.
    await bypassBtn.click()
    await expect(dialog).toBeVisible()
    await dialog.locator('[data-testid="confirm-dialog-confirm"]').click()
    await expect(dialog).toHaveCount(0)
    await expect.poll(() => bypassState()).toBe(true)

    await page.getByRole('button', { name: '← Back' }).click()
    await expect(page.locator('[data-testid="settings-bypass-badge"]')).toBeVisible()
  } finally {
    // Host-wide setting — never leave it loosened for the rest of the suite.
    await request.put('/api/settings/claude-permissions', {
      headers: auth,
      data: { bypass: false },
    })
  }
})
