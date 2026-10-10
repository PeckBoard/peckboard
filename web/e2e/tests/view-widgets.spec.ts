import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Saved Views as a widget dashboard: a Project widget added through the
 * header combobox summarises the board and links to it, a view stored as a
 * legacy split tree renders as widgets, and removing a widget persists.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type Auth = { token: string; auth: Record<string, string> }
type Widget = {
  id: string
  kind: string
  x: number
  y: number
  w: number
  h: number
  sessionId?: string | null
  projectId?: string | null
}

async function authenticate(request: APIRequestContext): Promise<Auth> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function createFolder(request: APIRequestContext, auth: Auth, name: string) {
  const res = await request.post('/api/folders', {
    headers: auth.auth,
    data: { name: `${name}-${Date.now()}`, path: mkdtempSync(path.join(tmpdir(), `pb-${name}-`)) },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function createSession(
  request: APIRequestContext,
  auth: Auth,
  folderId: string,
  name: string,
) {
  const res = await request.post('/api/sessions', {
    headers: auth.auth,
    data: { name, folder_id: folderId },
  })
  expect(res.ok(), `create session failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function createView(request: APIRequestContext, auth: Auth, data: object) {
  const res = await request.post('/api/me/views', { headers: auth.auth, data })
  expect(res.status(), await res.text()).toBe(201)
  return ((await res.json()) as { id: string }).id
}

async function viewWidgets(request: APIRequestContext, auth: Auth, viewId: string) {
  const res = await request.get(`/api/me/views/${viewId}`, { headers: auth.auth })
  expect(res.ok()).toBeTruthy()
  return ((await res.json()) as { widgets: Widget[] }).widgets
}

async function openView(page: Page, token: string, viewId: string) {
  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto(`/views/${viewId}`)
  await expect(page.getByTestId('view-editor')).toBeVisible({ timeout: 15_000 })
}

test('a project widget added from the combobox summarises the board and opens it', async ({
  request,
  page,
}) => {
  await page.setViewportSize({ width: 1400, height: 900 })
  const auth = await authenticate(request)
  const folder = await createFolder(request, auth, 'widget-project')
  const name = `widget board ${Date.now()}`
  // No workers, so the orchestrator never moves the seeded cards.
  const pres = await request.post('/api/projects', {
    headers: auth.auth,
    data: { name, folder_id: folder, worker_count: 0, workflow: 'task' },
  })
  expect(pres.ok(), await pres.text()).toBeTruthy()
  const projectId = ((await pres.json()) as { id: string }).id
  for (const [title, step] of [
    ['first backlog', 'backlog'],
    ['second backlog', 'backlog'],
    ['finished', 'done'],
  ]) {
    const c = await request.post(`/api/projects/${projectId}/cards`, {
      headers: auth.auth,
      data: { title, description: '', step, priority: 2 },
    })
    expect(c.ok(), await c.text()).toBeTruthy()
  }

  const viewId = await createView(request, auth, { name: 'project dash', widgets: [] })
  await openView(page, auth.token, viewId)
  await expect(page.getByTestId('view-empty')).toBeVisible()

  await page.getByTestId('add-widget-button').click()
  await page.getByTestId('view-add-project').click()
  await page.getByTestId('view-add-project-search').fill(name)
  await page.getByTestId(`view-add-project-option-${projectId}`).click()

  const widget = page.locator(`[data-testid="view-widget"][data-kind="project"]`)
  await expect(widget).toHaveCount(1)
  await expect(widget).toHaveAttribute('data-project-id', projectId)
  const steps = widget.getByTestId('project-widget-steps')
  await expect(steps.locator('li[data-step="backlog"]')).toContainText('2', { timeout: 15_000 })
  await expect(steps.locator('li[data-step="done"]')).toContainText('1')
  await expect(steps.locator('li[data-step="in_progress"]')).toContainText('0')
  await expect(widget.getByTestId('project-widget-status')).toHaveAttribute('data-status', /.+/)

  // Persisted as a project widget.
  await expect(page.getByTestId('view-save-state')).toHaveAttribute('data-state', 'saved', {
    timeout: 10_000,
  })
  const saved = await viewWidgets(request, auth, viewId)
  expect(saved).toHaveLength(1)
  expect(saved[0]).toMatchObject({ kind: 'project', projectId })

  await widget.getByTestId('project-widget-title').click()
  await expect(page).toHaveURL(new RegExp(`/projects/${projectId}`))
  await expect(page.locator('.kanban-card', { hasText: 'first backlog' })).toBeVisible({
    timeout: 15_000,
  })
})

test('a view saved as a legacy split tree renders as widgets', async ({ request, page }) => {
  await page.setViewportSize({ width: 1400, height: 900 })
  const auth = await authenticate(request)
  const folder = await createFolder(request, auth, 'widget-legacy')
  const a = await createSession(request, auth, folder, 'legacy left')
  const b = await createSession(request, auth, folder, 'legacy right')
  const viewId = await createView(request, auth, {
    name: 'legacy view',
    layout: {
      kind: 'split',
      dir: 'row',
      ratios: [0.5, 0.5],
      children: [
        { kind: 'leaf', sessionId: a },
        { kind: 'leaf', sessionId: b },
      ],
    },
  })

  const widgets = await viewWidgets(request, auth, viewId)
  expect(widgets).toHaveLength(2)
  const left = widgets.find((w) => w.sessionId === a)!
  const right = widgets.find((w) => w.sessionId === b)!
  expect(left).toMatchObject({ kind: 'session', x: 0, y: 0 })
  expect(right).toMatchObject({ kind: 'session', x: left.w, y: 0 })
  expect(left.w + right.w).toBe(12)

  await openView(page, auth.token, viewId)
  await expect(page.getByTestId('view-widget-grid')).toHaveAttribute('data-mode', 'grid')
  const la = page.locator(`[data-testid="view-widget"][data-pane-id="${a}"]`)
  const lb = page.locator(`[data-testid="view-widget"][data-pane-id="${b}"]`)
  await expect(la).toHaveAttribute('data-kind', 'session')
  await expect(lb).toBeVisible()
  const ba = (await la.boundingBox())!
  const bb = (await lb.boundingBox())!
  expect(Math.abs(ba.y - bb.y)).toBeLessThan(2)
  expect(bb.x).toBeGreaterThan(ba.x + ba.width - 2)
})

test('removing a widget persists across reload', async ({ request, page }) => {
  await page.setViewportSize({ width: 1400, height: 900 })
  const auth = await authenticate(request)
  const folder = await createFolder(request, auth, 'widget-remove')
  const keep = await createSession(request, auth, folder, 'keep me')
  const drop = await createSession(request, auth, folder, 'drop me')
  const viewId = await createView(request, auth, {
    name: 'remove view',
    widgets: [
      { id: 'w-keep', kind: 'session', x: 0, y: 0, w: 6, h: 10, sessionId: keep },
      { id: 'w-drop', kind: 'session', x: 6, y: 0, w: 6, h: 10, sessionId: drop },
    ],
  })

  await openView(page, auth.token, viewId)
  const widgets = page.getByTestId('view-widget')
  await expect(widgets).toHaveCount(2)
  await page
    .locator('[data-testid="view-widget"][data-widget-id="w-drop"]')
    .getByTestId('widget-menu')
    .click()
  await page.getByTestId('widget-remove').click()
  await expect(widgets).toHaveCount(1)
  await expect(widgets).toHaveAttribute('data-widget-id', 'w-keep')

  await expect(page.getByTestId('view-save-state')).toHaveAttribute('data-state', 'saved', {
    timeout: 10_000,
  })
  await expect
    .poll(async () => (await viewWidgets(request, auth, viewId)).map((w) => w.id))
    .toEqual(['w-keep'])

  await page.reload()
  await expect(page.getByTestId('view-editor')).toBeVisible({ timeout: 15_000 })
  await expect(widgets).toHaveCount(1)
  await expect(page.locator(`[data-pane-id="${drop}"]`)).toHaveCount(0)
})
