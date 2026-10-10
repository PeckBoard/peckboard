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
  filters?: Record<string, string | boolean | string[]> | null
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

async function createOrchestrator(request: APIRequestContext, auth: Auth, name: string) {
  const dir = mkdtempSync(path.join(tmpdir(), 'pb-orch-'))
  const fres = await request.post('/api/folders', {
    headers: auth.auth,
    data: { name: `orch-${Date.now()}`, path: dir },
  })
  expect(fres.ok(), `create folder failed: ${await fres.text()}`).toBeTruthy()
  const folderId = ((await fres.json()) as { id: string }).id
  // Disabled: no scheduled / watchdog fires, only the widget's Run now.
  const res = await request.post('/api/plugin-ui/session-control/orchestrators', {
    headers: auth.auth,
    data: {
      name,
      folder_id: folderId,
      goal: 'Ship the dashboard widgets with tests and screenshots',
      model: 'mock:happy-path',
      prompt: '{{goal}}',
      enabled: false,
    },
  })
  expect(res.ok(), `create orchestrator failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

type OrchList = {
  orchestrators: { id: string; paused: boolean; stats: { fires: number } }[]
  clock: string
}

async function listOrchestrators(request: APIRequestContext, auth: Auth): Promise<OrchList> {
  const res = await request.get('/api/plugin-ui/session-control/orchestrators', {
    headers: auth.auth,
  })
  expect(res.ok(), await res.text()).toBeTruthy()
  return (await res.json()) as OrchList
}

test('orchestrators widget runs and pauses; PRs widget explains a missing plugin', async ({
  request,
  page,
}) => {
  test.setTimeout(90_000)
  await page.setViewportSize({ width: 1400, height: 900 })
  const auth = await authenticate(request)
  const name = `Widget brain ${Date.now()}`
  const id = await createOrchestrator(request, auth, name)
  const orch = async () =>
    (await listOrchestrators(request, auth)).orchestrators.find((o) => o.id === id)!
  // Run now needs the engine clock, set on the plugin's first timer tick.
  await expect
    .poll(async () => (await listOrchestrators(request, auth)).clock, { timeout: 30_000 })
    .not.toBe('')

  const viewId = await createView(request, auth)
  await openView(page, auth.token, viewId)
  // session-control is bundled + preinstalled; github-bridge isn't installed.
  await page.getByTestId('add-widget-button').click()
  await expect(page.getByTestId('view-add-orchestrators')).toBeVisible()
  await expect(page.getByTestId('view-add-prs')).toHaveCount(0)
  await page.getByTestId('view-add-orchestrators').click()

  const w = widgetOf(page, 'orchestrators')
  const row = w.locator(`[data-testid="dash-orchestrator-row"][data-orch-id="${id}"]`)
  await expect(row).toContainText(name, { timeout: 15_000 })
  await expect(row).toContainText('Ship the dashboard widgets')
  await expect(row.getByTestId('dash-orchestrator-goal-status')).toHaveText('In progress')

  await row.getByTestId('dash-orchestrator-run').click()
  await expect(row).toContainText('Fired', { timeout: 15_000 })
  await expect.poll(async () => (await orch()).stats.fires).toBeGreaterThan(0)

  await row.getByTestId('dash-orchestrator-pause').click()
  await expect(row.getByTestId('dash-orchestrator-pause')).toHaveText('Resume')
  expect((await orch()).paused).toBe(true)
  await row.getByTestId('dash-orchestrator-pause').click()
  await expect(row.getByTestId('dash-orchestrator-pause')).toHaveText('Pause')
  expect((await orch()).paused).toBe(false)

  // Expanding the row shows recent activity (newest first).
  await row.locator('.orch-main').click()
  await expect(row.getByTestId('dash-orchestrator-activity')).toContainText('user_paused')

  // A saved prs widget (e.g. from before the plugin was removed) says why.
  const put = await request.put(`/api/me/views/${viewId}`, {
    headers: auth.auth,
    data: { widgets: [{ id: 'w-prs', kind: 'prs', x: 0, y: 0, w: 6, h: 8 }] },
  })
  expect(put.ok(), await put.text()).toBeTruthy()
  await page.reload()
  const prs = widgetOf(page, 'prs')
  await expect(prs.getByTestId('dash-prs-empty')).toContainText(
    'GitHub Bridge plugin not installed',
    { timeout: 15_000 },
  )
  // Installed but no GitHub token: a pointer to the plugin settings.
  await page.route('**/api/plugin-ui/github-bridge/prs', (route) =>
    route.fulfill({ json: { configured: false, prs: [], error: null } }),
  )
  await page.reload()
  await expect(prs.getByTestId('dash-prs-empty')).toContainText(
    'Connect GitHub in the GitHub Bridge plugin settings',
    { timeout: 15_000 },
  )
})

/** A populated `/prs` answer for screenshots (the plugin isn't in e2e). */
function fakePrs(projectId: string) {
  const ago = (m: number) => new Date(Date.now() - m * 60_000).toISOString()
  const pr = (o: Record<string, unknown>) => ({
    card_id: null,
    card_title: null,
    project_id: projectId,
    repo: 'peckboard/peckboard',
    draft: false,
    state: 'open',
    author: 'octocat',
    review: null,
    checks: { state: 'success', passed: 12, failed: 0, pending: 0 },
    ...o,
    url: `https://github.com/peckboard/peckboard/pull/${o.number}`,
  })
  return {
    configured: true,
    error: null,
    prs: [
      pr({
        number: 412,
        title: 'Dashboard: PRs & CI widget',
        updated_at: ago(3),
        card_id: 'c1',
        card_title: 'PR widget',
        checks: { state: 'pending', passed: 9, failed: 0, pending: 3 },
        review: 'review_required',
      }),
      pr({
        number: 409,
        title: 'Fix flaky worktree merge retry',
        updated_at: ago(25),
        card_title: 'Merge retry',
        checks: { state: 'failure', passed: 10, failed: 2, pending: 0 },
        review: 'changes_requested',
        author: 'hubot',
      }),
      pr({
        number: 401,
        title: 'Orchestrator goal status pills',
        updated_at: ago(90),
        review: 'approved',
      }),
      pr({
        number: 398,
        title: 'WIP: kokoro voice picker',
        updated_at: ago(300),
        draft: true,
        checks: { state: 'none', passed: 0, failed: 0, pending: 0 },
      }),
      pr({ number: 377, title: 'Bump diesel to 2.3', updated_at: ago(1500), state: 'merged' }),
      pr({
        repo: 'peckboard/plugins',
        number: 51,
        title: 'Superseded registry layout',
        updated_at: ago(4000),
        state: 'closed',
        checks: { state: 'none', passed: 0, failed: 0, pending: 0 },
      }),
    ],
  }
}

for (const theme of ['dark', 'light'] as const) {
  test(`screenshot: plugin widgets, ${theme} theme`, async ({ request, page }) => {
    test.skip(!process.env.PB_WIDGET_SHOTS, 'set PB_WIDGET_SHOTS=<dir> to capture')
    await page.setViewportSize({ width: 1400, height: 760 })
    const auth = await authenticate(request)
    const project = await createProject(request, auth, `plug-${theme}`)
    await createOrchestrator(request, auth, `Release captain (${theme})`)
    await page.route('**/api/plugin-ui/github-bridge/prs', (route) =>
      route.fulfill({ json: fakePrs(project.id) }),
    )
    const viewId = await createView(request, auth, [
      { id: 'w-prs', kind: 'prs', x: 0, y: 0, w: 6, h: 10 },
      { id: 'w-orch', kind: 'orchestrators', x: 6, y: 0, w: 6, h: 10 },
    ])
    await openView(page, auth.token, viewId, theme)
    await expect(widgetOf(page, 'prs').getByTestId('dash-prs-list')).toBeVisible({
      timeout: 15_000,
    })
    const row = widgetOf(page, 'orchestrators').getByTestId('dash-orchestrator-row').first()
    await expect(row).toBeVisible({ timeout: 15_000 })
    await row.locator('.orch-main').click()
    await expect(page.locator('[aria-busy="true"]')).toHaveCount(0, { timeout: 15_000 })
    await page.screenshot({
      path: path.join(process.env.PB_WIDGET_SHOTS!, `plugin-widgets-${theme}.png`),
    })
  })
}

test('needs attention kind chips and search filter rows and persist across reload', async ({
  request,
  page,
}) => {
  await page.setViewportSize({ width: 1400, height: 900 })
  const auth = await authenticate(request)
  const project = await createProject(request, auth, 'attn-filter')
  const alpha = `alpha stuck ${Date.now()}`
  const beta = `beta stuck ${Date.now()}`
  await createCard(request, auth, project.id, { title: alpha, blocked: true })
  await createCard(request, auth, project.id, { title: beta, blocked: true })

  const viewId = await createView(request, auth, [
    { id: 'w-attn', kind: 'attention', x: 0, y: 0, w: 6, h: 10, projectId: project.id },
  ])
  await openView(page, auth.token, viewId)

  const attn = widgetOf(page, 'attention')
  const list = attn.getByTestId('dash-attention-list')
  await expect(list).toContainText(alpha, { timeout: 15_000 })
  await expect(list).toContainText(beta)

  // The bar starts closed with no filters active.
  await expect(attn.getByTestId('widget-filter-bar')).toHaveCount(0)
  await attn.getByTestId('widget-filter-button').click()
  const kind = attn.getByTestId('widget-filter-kind')
  // Questions only: the blocked cards drop out.
  await kind.locator('[data-value="question"]').click()
  await expect(attn.getByTestId('dash-filter-nomatch')).toBeVisible()
  // Adding Blocked brings them back; search narrows to one.
  await kind.locator('[data-value="blocked"]').click()
  await expect(list).toContainText(beta)
  await attn.getByTestId('widget-filter-q').fill('alpha')
  await expect(list).not.toContainText(beta)
  await expect(list).toContainText(alpha)

  await expectSaved(page)
  await expect
    .poll(
      async () =>
        ((await viewWidgets(request, auth, viewId))[0] as { filters?: unknown }).filters ?? null,
    )
    .toEqual({ kind: ['question', 'blocked'], q: 'alpha' })

  await page.reload()
  // Active filters reopen the bar and still apply.
  await expect(attn.getByTestId('widget-filter-bar')).toBeVisible({ timeout: 15_000 })
  await expect(list).toContainText(alpha, { timeout: 15_000 })
  await expect(list).not.toContainText(beta)
  await expect(attn.getByTestId('widget-filter-q')).toHaveValue('alpha')
  await expect(attn.getByTestId('widget-filter-button')).toContainText('2')

  // Clear right after typing (inside the search debounce) must not let the
  // pending keystroke re-save once the clear has landed.
  await attn.getByTestId('widget-filter-q').fill('pending-text')
  await attn.getByTestId('widget-filter-clear').click()
  await expect(list).toContainText(beta)
  await expect(attn.getByTestId('widget-filter-q')).toHaveValue('')
  // Outlast the 200ms debounce: what's checked is that nothing arrives late.
  await page.waitForTimeout(600)
  await expectSaved(page)
  await expect
    .poll(
      async () =>
        ((await viewWidgets(request, auth, viewId))[0] as { filters?: unknown }).filters ?? null,
    )
    .toBeNull()
  expect(
    ((await viewWidgets(request, auth, viewId))[0] as { filters?: unknown }).filters ?? null,
  ).toBeNull()
})

/** Worker todos for the todos-filter test: real todos only arrive from a
 *  worker's TodoWrite stream, so the endpoint is stubbed. */
const TODOS = {
  cards: [
    {
      card_id: 'c-api',
      card_title: 'Build the API',
      todos: [
        { content: 'Write migration', status: 'completed' },
        { content: 'Add route handler', status: 'in_progress', activeForm: 'Adding route handler' },
        { content: 'Write route tests', status: 'pending' },
      ],
    },
    {
      card_id: 'c-ui',
      card_title: 'Polish the UI',
      todos: [
        { content: 'Fix dark theme contrast', status: 'pending' },
        { content: 'Ship screenshots', status: 'completed' },
      ],
    },
  ],
}

test('todos status chips and search filter items and persist across reload', async ({
  request,
  page,
}) => {
  await page.setViewportSize({ width: 1400, height: 900 })
  const auth = await authenticate(request)
  const project = await createProject(request, auth, 'todos-filter')
  await page.route('**/api/projects/*/todos', (r) => r.fulfill({ json: TODOS }))
  const viewId = await createView(request, auth, [
    { id: 'w-todos', kind: 'todos', x: 0, y: 0, w: 5, h: 10, projectId: project.id },
  ])
  await openView(page, auth.token, viewId)

  const todos = widgetOf(page, 'todos')
  const list = todos.getByTestId('dash-todos-list')
  await expect(list).toContainText('Write migration', { timeout: 15_000 })
  await expect(todos.getByTestId('widget-filter-bar')).toHaveCount(0)

  await todos.getByTestId('widget-filter-button').click()
  await todos.getByTestId('widget-filter-status').locator('[data-value="pending"]').click()
  await expect(list).toContainText('Write route tests')
  await expect(list).toContainText('Fix dark theme contrast')
  await expect(list).not.toContainText('Write migration')
  await expect(list).not.toContainText('Adding route handler')
  await todos.getByTestId('widget-filter-q').fill('route')
  await expect(list).not.toContainText('Fix dark theme contrast')
  await expect(list).toContainText('Write route tests')

  await expectSaved(page)
  const savedFilters = async () =>
    ((await viewWidgets(request, auth, viewId))[0] as { filters?: unknown }).filters ?? null
  await expect.poll(savedFilters).toEqual({ status: ['pending'], q: 'route' })

  await page.reload()
  await expect(todos.getByTestId('widget-filter-bar')).toBeVisible({ timeout: 15_000 })
  await expect(list).toContainText('Write route tests', { timeout: 15_000 })
  await expect(list).not.toContainText('Fix dark theme contrast')
  await expect(list).not.toContainText('Write migration')
  await expect(todos.getByTestId('widget-filter-q')).toHaveValue('route')
  await expect(todos.getByTestId('widget-filter-button')).toContainText('2')

  // A search nothing matches offers a one-click reset.
  await todos.getByTestId('widget-filter-q').fill('zzz-nothing')
  await expect(todos.getByTestId('dash-filter-nomatch')).toBeVisible()
  await todos.getByTestId('widget-filter-clear').click()
  await expect(list).toContainText('Write migration')
  await expect(list).toContainText('Fix dark theme contrast')
  await expect(todos.getByTestId('widget-filter-q')).toHaveValue('')
  await expectSaved(page)
  await expect.poll(savedFilters).toBeNull()
})

const BG_TASKS = {
  tasks: [
    ['t1', 's1', 'Build release', 'cargo build --release', 'running', null],
    ['t2', 's1', 'Build release', 'npm run build', 'succeeded', 0],
    ['t3', 's2', 'Fix flaky e2e', 'npx playwright test', 'failed', 1],
    ['t4', 's2', 'Fix flaky e2e', 'scripts/e2e-shards.sh 4', 'failed', 2],
  ].map(([id, sid, sname, program, status, exit]) => ({
    id,
    session_id: sid,
    session_name: sname,
    label: program,
    program,
    status,
    exit_code: exit,
    started_at: new Date(Date.now() - 300_000).toISOString(),
    finished_at: status === 'running' ? null : new Date(Date.now() - 60_000).toISOString(),
    stopping: false,
  })),
}

for (const theme of ['dark', 'light'] as const) {
  test(`screenshot: filtered widgets, ${theme} theme`, async ({ request, page }) => {
    test.skip(!process.env.PB_WIDGET_SHOTS, 'set PB_WIDGET_SHOTS=<dir> to capture')
    await page.setViewportSize({ width: 1400, height: 800 })
    const auth = await authenticate(request)
    const project = await createProject(request, auth, `fshots-${theme}`)
    await page.route('**/api/projects/*/todos', (r) => r.fulfill({ json: TODOS }))
    await page.route(/\/api\/dashboard\/background(\?|$)/, (r) => r.fulfill({ json: BG_TASKS }))
    const viewId = await createView(request, auth, [
      {
        id: 'w-bg',
        kind: 'background',
        x: 0,
        y: 0,
        w: 5,
        h: 9,
        filters: { status: ['failed'], q: 'e2e' },
      },
      {
        id: 'w-todos',
        kind: 'todos',
        x: 5,
        y: 0,
        w: 3,
        h: 9,
        projectId: project.id,
        filters: { status: ['pending', 'in_progress'] },
      },
      { id: 'w-rep', kind: 'repeating', x: 8, y: 0, w: 4, h: 9, filters: { state: 'enabled' } },
    ])
    await openView(page, auth.token, viewId, theme)
    await expect(widgetOf(page, 'background').getByTestId('dash-background-list')).toBeVisible({
      timeout: 15_000,
    })
    await expect(page.getByTestId('widget-filter-bar')).toHaveCount(3)
    await expect(page.locator('[aria-busy="true"]')).toHaveCount(0, { timeout: 15_000 })
    const dir = process.env.PB_WIDGET_SHOTS!
    await page.screenshot({ path: path.join(dir, `filters-${theme}.png`) })
    // The combo picker open, over the background widget.
    await widgetOf(page, 'background').getByTestId('widget-filter-session').click()
    await page.screenshot({ path: path.join(dir, `filters-combo-${theme}.png`) })
  })
}
