import { test, expect, type APIRequestContext, type Locator, type Page } from '../harness'
import { execFileSync } from 'node:child_process'
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Dashboard info widgets on the Views page: the grouped Add-widget menu,
 * notes, Needs Attention scoping, the card dependency graph, git commits,
 * every remaining kind rendering without an error, and server-side
 * validation of per-kind fields.
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
  projectId?: string | null
  cardId?: string | null
  body?: string | null
  hostRef?: string | null
}

async function authenticate(request: APIRequestContext): Promise<Auth> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function createProject(
  request: APIRequestContext,
  auth: Auth,
  label: string,
  dir = mkdtempSync(path.join(tmpdir(), `pb-${label}-`)),
) {
  const fres = await request.post('/api/folders', {
    headers: auth.auth,
    data: { name: `${label}-${Date.now()}`, path: dir },
  })
  expect(fres.ok(), `create folder failed: ${await fres.text()}`).toBeTruthy()
  const folderId = ((await fres.json()) as { id: string }).id
  const name = `${label} ${Date.now()}`
  // No workers, so the orchestrator never moves the seeded cards.
  const pres = await request.post('/api/projects', {
    headers: auth.auth,
    data: { name, folder_id: folderId, worker_count: 0, workflow: 'task' },
  })
  expect(pres.ok(), await pres.text()).toBeTruthy()
  return { id: ((await pres.json()) as { id: string }).id, name }
}

async function createCard(
  request: APIRequestContext,
  auth: Auth,
  projectId: string,
  data: Record<string, unknown>,
) {
  const res = await request.post(`/api/projects/${projectId}/cards`, {
    headers: auth.auth,
    data: { description: 'e2e', step: 'backlog', priority: 2, ...data },
  })
  expect(res.ok(), `create card failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function createView(request: APIRequestContext, auth: Auth, widgets: Widget[] = []) {
  const res = await request.post('/api/me/views', {
    headers: auth.auth,
    data: { name: `dash ${Date.now()}`, widgets },
  })
  expect(res.status(), await res.text()).toBe(201)
  return ((await res.json()) as { id: string }).id
}

async function viewWidgets(request: APIRequestContext, auth: Auth, viewId: string) {
  const res = await request.get(`/api/me/views/${viewId}`, { headers: auth.auth })
  expect(res.ok()).toBeTruthy()
  return ((await res.json()) as { widgets: Widget[] }).widgets
}

async function openView(page: Page, token: string, viewId: string, theme?: 'dark' | 'light') {
  await page.addInitScript(
    ([t, th]) => {
      localStorage.setItem('peckboard_token', t)
      if (th) localStorage.setItem('peckboard_theme', th)
    },
    [token, theme ?? ''] as const,
  )
  await page.goto(`/views/${viewId}`)
  await expect(page.getByTestId('view-editor')).toBeVisible({ timeout: 15_000 })
}

const widgetOf = (page: Page, kind: string): Locator =>
  page.locator(`[data-testid="view-widget"][data-kind="${kind}"]`)

/** The widget settled into its list or empty state, without an error. */
async function expectSettled(widget: Locator, kind: string) {
  const ok = widget.locator(
    `[data-testid="dash-${kind}-list"], [data-testid="dash-${kind}-empty"], [data-testid="dash-${kind}-body"]`,
  )
  await expect(ok.first(), `${kind} renders a list or empty state`).toBeVisible({
    timeout: 15_000,
  })
  await expect(widget.locator('[role="alert"]'), `${kind} shows no error`).toHaveCount(0)
}

async function expectSaved(page: Page) {
  await expect(page.getByTestId('view-save-state')).toHaveAttribute('data-state', 'saved', {
    timeout: 10_000,
  })
}

test('the add menu is grouped, and a note typed in place persists across reload', async ({
  request,
  page,
}) => {
  await page.setViewportSize({ width: 1400, height: 900 })
  const auth = await authenticate(request)
  const viewId = await createView(request, auth)
  await openView(page, auth.token, viewId)

  await page.getByTestId('add-widget-button').click()
  const headings = page.locator('.dropdown-group-label')
  await expect(headings).toHaveText([
    'Panes',
    'Project',
    'Activity',
    'Quality',
    'Notes',
    'Infrastructure',
  ])
  // e2e preinstalls every bundled plugin, ssh-fleet included, so its kinds show.
  await expect(page.getByTestId('view-add-ssh-activity')).toBeVisible()
  await expect(page.getByTestId('view-add-ssh-hosts')).toBeVisible()
  await page.getByTestId('view-add-note').click()

  const note = widgetOf(page, 'note')
  await expect(note).toHaveCount(1)
  await note.getByTestId('dash-note-start').click()
  const textarea = note.getByTestId('dash-note-textarea')
  await textarea.fill('# Standup\n\n- ship **widgets**')
  await textarea.blur()
  await expect(note.getByTestId('dash-note-body').locator('strong')).toHaveText('widgets')
  await expect(note.locator('.widget-title')).toHaveText('Standup')

  await expectSaved(page)
  await expect
    .poll(async () => (await viewWidgets(request, auth, viewId))[0]?.body)
    .toBe('# Standup\n\n- ship **widgets**')

  await page.reload()
  await expect(note.getByTestId('dash-note-body').locator('li')).toHaveText('ship widgets', {
    timeout: 15_000,
  })
})

test('needs attention lists a blocked card and scopes to one project', async ({
  request,
  page,
}) => {
  await page.setViewportSize({ width: 1400, height: 900 })
  const auth = await authenticate(request)
  const busy = await createProject(request, auth, 'attn-busy')
  const quiet = await createProject(request, auth, 'attn-quiet')
  const title = `stuck card ${Date.now()}`
  await createCard(request, auth, busy.id, {
    title,
    blocked: true,
    block_reason: 'waiting on credentials',
  })

  const viewId = await createView(request, auth)
  await openView(page, auth.token, viewId)
  await page.getByTestId('add-widget-button').click()
  await page.getByTestId('view-add-attention').click()

  const attn = widgetOf(page, 'attention')
  const list = attn.getByTestId('dash-attention-list')
  await expect(list.locator('[data-group="blocked"]')).toContainText(title, { timeout: 15_000 })
  await expect(list).toContainText('waiting on credentials')

  await attn.getByTestId('widget-menu').click()
  await page.getByTestId('widget-configure').click()
  await expect(page.getByTestId('widget-config-modal')).toBeVisible()
  await page.getByTestId('widget-config-project').click()
  await page.getByTestId('widget-config-project-search').fill(quiet.name)
  await page.getByTestId(`widget-config-project-option-${quiet.id}`).click()
  await page.getByTestId('widget-config-done').click()

  await expect(attn.getByTestId('dash-attention-empty')).toBeVisible({ timeout: 15_000 })
  await expectSaved(page)
  await expect
    .poll(async () => (await viewWidgets(request, auth, viewId))[0]?.projectId)
    .toBe(quiet.id)
})

test('card dependencies rank the root blocker and draw the graph', async ({ request, page }) => {
  await page.setViewportSize({ width: 1400, height: 900 })
  const auth = await authenticate(request)
  const project = await createProject(request, auth, 'deps')
  const root = await createCard(request, auth, project.id, { title: 'Root schema' })
  const mid = await createCard(request, auth, project.id, {
    title: 'Mid API',
    depends_on: [root],
  })
  await createCard(request, auth, project.id, { title: 'Leaf UI', depends_on: [mid] })

  const viewId = await createView(request, auth, [
    { id: 'w-deps', kind: 'dependencies', x: 0, y: 0, w: 8, h: 12, projectId: project.id },
  ])
  await openView(page, auth.token, viewId)

  const deps = widgetOf(page, 'dependencies')
  const list = deps.getByTestId('dash-dependencies-list')
  await expect(list).toBeVisible({ timeout: 15_000 })
  await expect(list.locator(`li[data-card-id="${root}"]`)).toContainText('blocks 2')
  await expect(list.locator(`li[data-card-id="${mid}"]`)).toContainText('blocks 1')
  await expect(list.locator('.dash-dag-node')).toHaveCount(3)
  await expect(list.locator('.dash-dag-edge')).toHaveCount(2)
})

test('git / worktrees lists recent commits of the project folder', async ({ request, page }) => {
  await page.setViewportSize({ width: 1400, height: 900 })
  const auth = await authenticate(request)
  const repo = mkdtempSync(path.join(tmpdir(), 'pb-git-'))
  const git = (...args: string[]) =>
    execFileSync('git', ['-c', 'user.name=E2E Bot', '-c', 'user.email=e2e@example.com', ...args], {
      cwd: repo,
      stdio: 'pipe',
    })
  git('init', '-q')
  mkdirSync(path.join(repo, 'src'))
  writeFileSync(path.join(repo, 'src', 'a.txt'), 'a\n')
  git('add', '.')
  git('commit', '-q', '-m', 'first widget commit')
  writeFileSync(path.join(repo, 'src', 'b.txt'), 'b\n')
  git('add', '.')
  git('commit', '-q', '-m', 'second widget commit')
  const project = await createProject(request, auth, 'git', repo)

  const viewId = await createView(request, auth, [
    { id: 'w-git', kind: 'worktrees', x: 0, y: 0, w: 6, h: 8, projectId: project.id },
  ])
  await openView(page, auth.token, viewId)

  const commits = widgetOf(page, 'worktrees').locator('.dash-commit')
  await expect(commits).toHaveCount(2, { timeout: 15_000 })
  await expect(commits.nth(0)).toContainText('second widget commit')
  await expect(commits.nth(1)).toContainText('first widget commit')
  await expect(commits.nth(0)).toContainText('E2E Bot')
})

/** One of every info kind, packed onto the 12-column grid. */
function allKinds(projectId: string): Widget[] {
  return [
    { id: 'w-attn', kind: 'attention', x: 0, y: 0, w: 4, h: 8 },
    { id: 'w-rq', kind: 'review_queue', x: 4, y: 0, w: 4, h: 8 },
    { id: 'w-todos', kind: 'todos', x: 8, y: 0, w: 4, h: 8, projectId },
    { id: 'w-deps', kind: 'dependencies', x: 0, y: 8, w: 6, h: 9, projectId },
    { id: 'w-workers', kind: 'workers', x: 6, y: 8, w: 6, h: 9 },
    { id: 'w-quality', kind: 'review_quality', x: 0, y: 17, w: 6, h: 7 },
    { id: 'w-git', kind: 'worktrees', x: 6, y: 17, w: 6, h: 7 },
    { id: 'w-bg', kind: 'background', x: 0, y: 24, w: 6, h: 6 },
    { id: 'w-rep', kind: 'repeating', x: 6, y: 24, w: 6, h: 6 },
    { id: 'w-note', kind: 'note', x: 0, y: 30, w: 4, h: 6, body: '## Pinned\n\nTry *everything*.' },
    { id: 'w-report', kind: 'report', x: 4, y: 30, w: 8, h: 6 },
    { id: 'w-sshact', kind: 'ssh_activity', x: 0, y: 36, w: 6, h: 8 },
    { id: 'w-sshhosts', kind: 'ssh_hosts', x: 6, y: 36, w: 6, h: 8 },
  ]
}

test('every info widget renders its list or empty state without errors', async ({
  request,
  page,
}) => {
  await page.setViewportSize({ width: 1400, height: 900 })
  const auth = await authenticate(request)
  const project = await createProject(request, auth, 'smoke')
  const viewId = await createView(request, auth)
  const put = await request.put(`/api/me/views/${viewId}`, {
    headers: auth.auth,
    data: { widgets: allKinds(project.id) },
  })
  expect(put.ok(), await put.text()).toBeTruthy()

  await openView(page, auth.token, viewId)
  await expect(page.getByTestId('view-widget')).toHaveCount(13)
  for (const kind of [
    'attention',
    'review_queue',
    'todos',
    'dependencies',
    'workers',
    'review_quality',
    'worktrees',
    'background',
    'repeating',
    'report',
  ]) {
    await expectSettled(widgetOf(page, kind), kind)
  }
  await expect(widgetOf(page, 'note').getByTestId('dash-note-body')).toContainText('everything')
  // ssh-fleet is installed (no hosts seeded): the SSH kinds reach the plugin.
  for (const kind of ['ssh_activity', 'ssh_hosts']) {
    const w = widgetOf(page, kind)
    await expectSettled(w, kind)
    await expect(w.locator('[data-reason="not-installed"]')).toHaveCount(0)
  }

  // Every kind and its fields survive a reload unchanged.
  await page.reload()
  await expect(page.getByTestId('view-widget')).toHaveCount(13, { timeout: 15_000 })
  const saved = await viewWidgets(request, auth, viewId)
  expect(saved.map((w) => w.kind).sort()).toEqual(
    allKinds(project.id)
      .map((w) => w.kind)
      .sort(),
  )
  expect(saved.find((w) => w.id === 'w-todos')).toMatchObject({ projectId: project.id })
  expect(saved.find((w) => w.id === 'w-note')?.body).toBe('## Pinned\n\nTry *everything*.')
})

test('the server rejects fields a widget kind does not use', async ({ request }) => {
  const auth = await authenticate(request)
  const viewId = await createView(request, auth)
  const put = (widgets: object[]) =>
    request.put(`/api/me/views/${viewId}`, { headers: auth.auth, data: { widgets } })

  const stray = await put([{ id: 'n', kind: 'note', x: 0, y: 0, w: 4, h: 6, hostRef: 'h1' }])
  expect(stray.status()).toBe(400)
  expect(await stray.text()).toContain('hostRef')

  const unknown = await put([{ id: 'u', kind: 'weather', x: 0, y: 0, w: 4, h: 6 }])
  expect(unknown.status()).toBe(400)

  const ok = await put([{ id: 's', kind: 'ssh_activity', x: 0, y: 0, w: 6, h: 8, hostRef: 'h1' }])
  expect(ok.ok(), await ok.text()).toBeTruthy()
  expect((await viewWidgets(request, auth, viewId))[0]).toMatchObject({ hostRef: 'h1' })
})

for (const theme of ['dark', 'light'] as const) {
  test(`screenshot: every widget, ${theme} theme`, async ({ request, page }) => {
    test.skip(!process.env.PB_WIDGET_SHOTS, 'set PB_WIDGET_SHOTS=<dir> to capture')
    await page.setViewportSize({ width: 1600, height: 1000 })
    const auth = await authenticate(request)
    const project = await createProject(request, auth, `shots-${theme}`)
    const a = await createCard(request, auth, project.id, { title: 'Design the schema' })
    const b = await createCard(request, auth, project.id, {
      title: 'Build the API layer',
      depends_on: [a],
    })
    await createCard(request, auth, project.id, { title: 'Wire up the UI', depends_on: [b] })
    await createCard(request, auth, project.id, {
      title: 'Blocked on vendor access',
      blocked: true,
      block_reason: 'waiting on credentials from the vendor',
    })
    const viewId = await createView(request, auth, allKinds(project.id))
    await openView(page, auth.token, viewId, theme)
    await expect(widgetOf(page, 'dependencies').getByTestId('dash-dependencies-list')).toBeVisible({
      timeout: 15_000,
    })
    await expect(page.locator('[aria-busy="true"]')).toHaveCount(0, { timeout: 15_000 })
    const dir = process.env.PB_WIDGET_SHOTS!
    await page.screenshot({ path: path.join(dir, `widgets-${theme}.png`) })
    // The grid scrolls inside the page; a tall viewport shows every row.
    await page.setViewportSize({ width: 1600, height: 2300 })
    await page.screenshot({ path: path.join(dir, `widgets-${theme}-full.png`) })
  })
}
