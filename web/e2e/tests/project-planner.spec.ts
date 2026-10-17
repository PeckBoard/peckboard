import { test, expect, type APIRequestContext } from '../harness'
import { spawnSync } from 'node:child_process'
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * project-planner plugin (staged + approved by the e2e harness):
 *
 *  - The Folders page offers a per-folder "Project Planner" button
 *    (manifest `folder_items`), which opens the sandboxed slideshow page.
 *  - The same button is offered on that folder's repo list (`/repos`), so
 *    opening Repos does not hide the planner.
 *  - The start slide renders with a model picker (thinking models only —
 *    the mock provider contributes "Mock: plan review (thinking)").
 *  - Beginning the interview creates + dispatches a real temp session and
 *    the page flips to the thinking state.
 *  - The mock model never calls the planner's MCP tools, so the plugin's
 *    stall watchdog nudges it and then fails the interview with its
 *    explicit message — proving the session wiring, event polling, and
 *    status machine work end to end. (The Q→A→definition cycle itself is
 *    unit-tested in peck-plugins/project-planner/test/planner.test.ts,
 *    where the tool calls can be driven directly.)
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { Authorization: `Bearer ${token}` }
}

async function loginUi(page: import('@playwright/test').Page, baseURL: string) {
  await page.goto(baseURL)
  // Auth occasionally restores before the form paints — only log in when
  // the form actually shows, then wait for the rail either way.
  const username = page.getByLabel('Username')
  await Promise.race([
    username.waitFor({ state: 'visible', timeout: 10_000 }).catch(() => {}),
    page
      .locator('.rail')
      .waitFor({ state: 'visible', timeout: 10_000 })
      .catch(() => {}),
  ])
  if (await username.isVisible().catch(() => false)) {
    await username.fill(E2E_USER)
    await page.getByLabel('Password').fill(E2E_PASS)
    await page.getByRole('button', { name: /sign in/i }).click()
  }
  await expect(page.locator('.rail')).toBeVisible()
}

test('slideshow page: start → thinking → watchdog verdict on a tool-less run', async ({
  page,
  baseURL,
  request,
}) => {
  expect(baseURL).toBeTruthy()
  const auth = await authenticate(request)

  // The plugin is staged from peck-plugins/project-planner/dist/plugin.wasm
  // at config-eval time; a checkout that never built it (each plugin is its
  // own repo) simply doesn't have the plugin — skip rather than fail.
  const catalogRes = await request.get('/api/plugins', { headers: auth })
  const catalog = catalogRes.ok() ? await catalogRes.json() : { plugins: [] }
  const staged = JSON.stringify(catalog).includes('project-planner')
  test.skip(
    !staged,
    'project-planner wasm not built/staged — run peck-plugins/project-planner/build.sh',
  )

  // Interviews are repo-scoped: the folder root is made a git repo so the
  // picker auto-selects it (a single-repo folder skips the picker).
  const folderName = `e2e-planner-${Date.now()}`
  const root = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-planner-'))
  const git = (args: string[]) => {
    const res = spawnSync('git', ['-C', root, ...args], { encoding: 'utf8' })
    expect(res.status, `git ${args.join(' ')}: ${res.stderr}`).toBe(0)
  }
  git(['init', '-b', 'main'])
  writeFileSync(path.join(root, 'README.md'), 'hello\n')
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: folderName, path: root },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  await loginUi(page, baseURL!)
  await page.locator('.rail-btn[title="Folders"]').click()

  // NO folder-level entry: the planner is repo_scoped, so the Folders row
  // menu must not offer it — the launch lives on the repo browser's repo rows.
  const folderRow = page
    .locator('.list-view-row')
    .filter({ has: page.getByTestId(`folder-row-${folderName}`) })
  await folderRow.locator('.list-view-menu').click()
  await expect(page.getByRole('menuitem', { name: 'Repos' })).toBeVisible()
  await expect(page.getByRole('menuitem', { name: 'Project Planner' })).toHaveCount(0)
  await page.keyboard.press('Escape')

  await page.getByTestId(`folder-row-${folderName}`).click()
  await expect(page.getByTestId('repo-list-view')).toBeVisible()
  await page.locator('.list-view-row .list-view-menu').first().click()
  await page.getByRole('menuitem', { name: 'Project Planner' }).click()
  await expect(page).toHaveURL(new RegExp(`/folders/${folder.id}/plugin/project-planner\\?repo=`))
  const frame = page.frameLocator('[data-testid="plugin-fullpage-frame"]')

  // Deep-linked to the repo: the start slide shows directly (no picker).
  await expect(frame.getByText('Plan this project, one question at a time')).toBeVisible({
    timeout: 15_000,
  })
  await expect(frame.getByText('Pick a repository')).toHaveCount(0)
  const select = frame.locator('.model-select')
  await expect(select).toBeVisible()
  await select.selectOption({ label: 'Mock: plan review (thinking)' })
  // Optional steer: a starting topic rides along in the start request.
  const topic = frame.locator('.topic-input')
  await expect(topic).toBeVisible()
  const startReq = page.waitForRequest(
    (r) => r.url().includes('/api/plugin-ui/project-planner/start') && r.method() === 'POST',
  )
  await topic.fill('game rules')
  await frame.getByRole('button', { name: 'Begin the interview' }).click()
  expect(((await startReq).postDataJSON() as { topic: string }).topic).toBe('game rules')

  // The interview is live: a real temp session was created and dispatched.
  await expect(frame.locator('.dots')).toBeVisible({ timeout: 15_000 })

  // The mock run ends without calling project_planner_ask; the watchdog
  // nudges it MAX_NUDGES times and then fails with its explicit verdict.
  await expect(frame.getByText('The interview stopped')).toBeVisible({ timeout: 60_000 })
  await expect(frame.getByText(/without showing a slide/)).toBeVisible()

  // Reset offers a clean restart.
  await frame.getByRole('button', { name: 'Reset and start again' }).click()
  await expect(frame.getByText('Plan this project, one question at a time')).toBeVisible({
    timeout: 15_000,
  })
})

test('repo rows offer the repo-scoped Project Planner; header does not', async ({
  page,
  baseURL,
  request,
}) => {
  expect(baseURL).toBeTruthy()
  const auth = await authenticate(request)

  const catalogRes = await request.get('/api/plugins', { headers: auth })
  const catalog = catalogRes.ok() ? await catalogRes.json() : { plugins: [] }
  test.skip(
    !JSON.stringify(catalog).includes('project-planner'),
    'project-planner wasm not built/staged — run peck-plugins/project-planner/build.sh',
  )

  const folderName = `e2e-planner-repos-${Date.now()}`
  // Repo-scoped planner: the folder must hold a git repo for the start
  // screen to appear (a single repo is auto-selected past the picker).
  const root = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-planner-'))
  const res = spawnSync('git', ['-C', root, 'init', '-b', 'main'], { encoding: 'utf8' })
  expect(res.status, res.stderr).toBe(0)
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: folderName, path: root },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  await loginUi(page, baseURL!)
  await loginUi(page, baseURL!)
  await page.locator('.rail-btn[title="Folders"]').click()
  await page.getByTestId(`folder-row-${folderName}`).click()
  await expect(page).toHaveURL(new RegExp(`/folders/${folder.id}/repos$`))
  await expect(page.getByTestId('repo-list-view')).toBeVisible()

  // repo_scoped: never on the folder-level header…
  await expect(page.getByTestId('repo-list-plugin-project-planner')).toHaveCount(0)
  // …but on each repo row's menu, opening the planner aimed at that repo.
  await page.locator('.list-view-row .list-view-menu').first().click()
  await page.getByRole('menuitem', { name: 'Project Planner' }).click()
  await expect(page).toHaveURL(new RegExp(`/folders/${folder.id}/plugin/project-planner\\?repo=`))
  const frame = page.frameLocator('[data-testid="plugin-fullpage-frame"]')
  await expect(frame.getByText('Plan this project, one question at a time')).toBeVisible({
    timeout: 15_000,
  })
})

test('interview session stays out of session lists and survives being viewed', async ({
  page,
  baseURL,
  request,
}) => {
  expect(baseURL).toBeTruthy()
  const auth = await authenticate(request)

  const catalogRes = await request.get('/api/plugins', { headers: auth })
  const catalog = catalogRes.ok() ? await catalogRes.json() : { plugins: [] }
  test.skip(
    !JSON.stringify(catalog).includes('project-planner'),
    'project-planner wasm not built/staged — run peck-plugins/project-planner/build.sh',
  )

  const folderName = `e2e-planner-hidden-${Date.now()}`
  const root = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-planner-'))
  const res = spawnSync('git', ['-C', root, 'init', '-b', 'main'], { encoding: 'utf8' })
  expect(res.status, res.stderr).toBe(0)
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: folderName, path: root },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  // Expert sessions (the planner's interview session is one) are still
  // reachable through the expert usage rollup — that's how the id is found.
  type Entity = { id: string; name: string }
  const plannerSessions = async (): Promise<Entity[]> => {
    const body = await (await request.get('/api/usage/experts', { headers: auth })).json()
    const rows = (Array.isArray(body) ? body : (body.items ?? [])) as Entity[]
    return rows.filter((r) => r.name.startsWith('Project Planner interview'))
  }
  const before = new Set((await plannerSessions()).map((s) => s.id))

  await loginUi(page, baseURL!)
  await page.goto(`/folders/${folder.id}/plugin/project-planner?repo=.`)
  const frame = page.frameLocator('[data-testid="plugin-fullpage-frame"]')
  await expect(frame.getByText('Plan this project, one question at a time')).toBeVisible({
    timeout: 15_000,
  })
  await frame.locator('.model-select').selectOption({ label: 'Mock: plan review (thinking)' })
  await frame.getByRole('button', { name: 'Begin the interview' }).click()
  await expect(frame.locator('.dots')).toBeVisible({ timeout: 15_000 })

  let id = ''
  await expect
    .poll(async () => {
      const created = (await plannerSessions()).find((s) => !before.has(s.id))
      if (created) id = created.id
      return !!created
    })
    .toBe(true)

  // Not in the sessions list (all folders, this folder, or a name search).
  const listed = async (query: string) =>
    (
      (await (await request.get(`/api/sessions?${query}`, { headers: auth })).json()) as {
        items: { id: string }[]
      }
    ).items.some((s) => s.id === id)
  expect(await listed('limit=500')).toBe(false)
  expect(await listed(`folder_id=${folder.id}&limit=500`)).toBe(false)
  expect(await listed(`q=${encodeURIComponent('Project Planner')}&limit=500`)).toBe(false)

  // Nor on the Sessions page (the default view).
  await page.goto('/')
  await expect(page.locator('.rail')).toBeVisible()
  await expect(
    page.locator('.list-view-row', { hasText: 'Project Planner interview' }),
  ).toHaveCount(0)

  // Viewing it via its URL opens it — and closing that tab must not delete
  // the session (the plugin is still driving it).
  await page.goto(`/sessions/${id}`)
  const chip = page.locator(`.tab-wrap[data-tab-id="session:${id}"]`)
  await expect(chip).toBeVisible({ timeout: 15_000 })
  await expect(chip.locator('.tab-icon-temp-session')).toHaveCount(0)
  await chip.locator('.tab-close').click()
  await expect(chip).toHaveCount(0)

  // Give a (wrong) delete-on-close time to land before asserting it didn't.
  await page.waitForTimeout(1_000)
  const after = await request.get(`/api/sessions/${id}`, { headers: auth })
  expect(after.status(), 'planner session must survive being viewed').toBe(200)
})

test('split definition: root index + section files are listed and browsable', async ({
  page,
  baseURL,
  request,
}) => {
  expect(baseURL).toBeTruthy()
  const auth = await authenticate(request)

  const catalogRes = await request.get('/api/plugins', { headers: auth })
  const catalog = catalogRes.ok() ? await catalogRes.json() : { plugins: [] }
  test.skip(
    !JSON.stringify(catalog).includes('project-planner'),
    'project-planner wasm not built/staged — run peck-plugins/project-planner/build.sh',
  )

  const folderName = `e2e-planner-split-${Date.now()}`
  const root = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-planner-'))
  const res = spawnSync('git', ['-C', root, 'init', '-b', 'main'], { encoding: 'utf8' })
  expect(res.status, res.stderr).toBe(0)
  // The layout the planner writes (the mock model never calls the planner's
  // tools, so the writes themselves are unit-tested): a root index plus two
  // section files, each depending on the root.
  const depends =
    '> Depends on: [PROJECT_DEFINITION.md](../PROJECT_DEFINITION.md) — this file is one section of that project definition; read it first.\n'
  writeFileSync(
    path.join(root, 'PROJECT_DEFINITION.md'),
    '# Hat Shop\n\n## Purpose\nSell hats online.\n\n## Sections\n\n' +
      '<!-- project-planner:sections — generated; edit the linked files, not this list -->\n\n' +
      '- Users & Stories — not written yet\n' +
      '- [Flows](project-definition/flows.md) — checkout flow\n' +
      '- Architecture — not written yet\n' +
      '- [Technology](project-definition/technology.md) — Rust\n' +
      '- Deployment & Operations — not written yet\n' +
      '- Open Questions — not written yet\n',
  )
  mkdirSync(path.join(root, 'project-definition'))
  writeFileSync(
    path.join(root, 'project-definition/flows.md'),
    `# Flows\n\n${depends}\n- FLOW-CHECKOUT: cart to payment.\n`,
  )
  writeFileSync(
    path.join(root, 'project-definition/technology.md'),
    `# Technology\n\n${depends}\n- TECH-RUST: one Rust binary.\n`,
  )
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: folderName, path: root },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  await loginUi(page, baseURL!)
  await page.goto(`/folders/${folder.id}/plugin/project-planner?repo=.`)
  const frame = page.frameLocator('[data-testid="plugin-fullpage-frame"]')
  await expect(frame.getByText('Plan this project, one question at a time')).toBeVisible({
    timeout: 15_000,
  })
  await expect(frame.locator('.file-chip')).toHaveText('PROJECT_DEFINITION.md + 2 section files')
  // The panel lists the root and all six sections, written or not.
  await frame.getByRole('button', { name: 'View definition' }).click()
  const tabs = frame.locator('.def-file')
  await expect(tabs).toHaveCount(7)
  await expect(frame.locator('.def-file.active')).toContainText('Overview')
  await expect(frame.locator('.def-view')).toContainText('Sell hats online.')
  await expect(frame.locator('.def-file[data-section="architecture"]')).toContainText(
    'not written yet',
  )
  // Two-column panel: the file list sits beside the viewer, not above it.
  const navBox = await frame.locator('.def-files').boundingBox()
  const viewBox = await frame.locator('.def-view').boundingBox()
  expect(navBox && viewBox && navBox.x + navBox.width <= viewBox.x).toBeTruthy()

  // A tab shows that one file; an index link jumps to its file.
  await frame.locator('.def-file[data-section="technology"]').click()
  await expect(frame.locator('.def-view')).toContainText('TECH-RUST')
  await expect(frame.locator('.def-view')).toContainText('Depends on')
  await frame.locator('.def-file[data-section="overview"]').click()
  await frame.locator('.def-view a.def-link', { hasText: 'Flows' }).click()
  await expect(frame.locator('.def-file.active')).toContainText('Flows')
  await page.screenshot({ path: '../tmp-pp-shot-flows.png', fullPage: true })
})

test('batch: every slide answered with no agent run between, then one record run', async ({
  page,
  baseURL,
  request,
}) => {
  expect(baseURL).toBeTruthy()
  const auth = await authenticate(request)

  const catalogRes = await request.get('/api/plugins', { headers: auth })
  const catalog = catalogRes.ok() ? await catalogRes.json() : { plugins: [] }
  test.skip(
    !JSON.stringify(catalog).includes('project-planner'),
    'project-planner wasm not built/staged — run peck-plugins/project-planner/build.sh',
  )

  const folderName = `e2e-planner-batch-${Date.now()}`
  const root = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-planner-'))
  const res = spawnSync('git', ['-C', root, 'init', '-b', 'main'], { encoding: 'utf8' })
  expect(res.status, res.stderr).toBe(0)
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: folderName, path: root },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const scoped = { ...auth, 'x-peckboard-folder-id': folder.id }

  type Entity = { id: string; name: string }
  const plannerSessions = async (): Promise<Entity[]> => {
    const body = await (await request.get('/api/usage/experts', { headers: auth })).json()
    const rows = (Array.isArray(body) ? body : (body.items ?? [])) as Entity[]
    return rows.filter((r) => r.name.startsWith('Project Planner interview'))
  }
  const before = new Set((await plannerSessions()).map((s) => s.id))

  // mock:block keeps the kickoff run alive, so its MCP token stays valid and
  // the watchdog never sees a run end — the test plays the agent's part
  // through the planner's MCP tools.
  const startRes = await request.post('/api/plugin-ui/project-planner/start', {
    headers: scoped,
    data: { repo: '.', model: 'mock:block' },
  })
  expect(startRes.ok(), `start failed: ${await startRes.text()}`).toBeTruthy()
  let sessionId = ''
  await expect
    .poll(async () => {
      const created = (await plannerSessions()).find((s) => !before.has(s.id))
      if (created) sessionId = created.id
      return !!created
    })
    .toBe(true)
  const cfgPath = path.join(process.env.PECKBOARD_E2E_DATA_DIR!, 'worker-mcp', `${sessionId}.json`)
  await expect.poll(() => existsSync(cfgPath), { timeout: 15_000 }).toBe(true)
  const mcpToken = (
    JSON.parse(readFileSync(cfgPath, 'utf8')) as {
      mcpServers: { peckboard: { headers: { Authorization: string } } }
    }
  ).mcpServers.peckboard.headers.Authorization.replace(/^Bearer /, '')
  const mcpCall = async (tool: string, args: Record<string, unknown>) => {
    const r = await request.post('/mcp', {
      headers: { Authorization: `Bearer ${mcpToken}` },
      data: {
        jsonrpc: '2.0',
        id: 1,
        method: 'tools/call',
        params: { name: tool, arguments: args },
      },
    })
    expect(r.ok()).toBeTruthy()
    const body = (await r.json()) as { result?: { content: { text?: string }[] }; error?: unknown }
    expect(body.error, `${tool}: ${JSON.stringify(body.error)}`).toBeFalsy()
    return body.result?.content?.[0]?.text ?? ''
  }

  // A dependent follow-up waits in the queue; the batch asks one queued
  // entry (from_queue) plus a fresh question.
  await mcpCall('project_planner_queue', { add: ['Which DB holds orders?', 'What should X do?'] })
  const asked = await mcpCall('project_planner_ask', {
    questions: [
      {
        topic: 'Purpose',
        section: 'overview',
        kind: 'fill',
        question: 'The project exists to ___.',
        why: 'Every other choice depends on it.',
      },
      {
        topic: 'Database',
        section: 'technology',
        kind: 'fill',
        question: 'Orders live in ___.',
        why: 'Storage shapes the stack.',
        from_queue: 'Which DB holds orders?',
      },
    ],
  })
  expect(asked).toMatch(/All 2 slides are showing/)

  await loginUi(page, baseURL!)
  await page.goto(`/folders/${folder.id}/plugin/project-planner?repo=.`)
  const frame = page.frameLocator('[data-testid="plugin-fullpage-frame"]')
  await expect(frame.locator('.batch-progress')).toHaveText('Question 1 of 2', { timeout: 15_000 })
  await expect(frame.locator('.queue-badge')).toHaveText('1 queued')
  await page.screenshot({ path: '../tmp-pp-shot-batch.png', fullPage: true })
  await frame.locator('.fill-input').fill('sell hats')
  await frame.getByRole('button', { name: 'Next question' }).click()

  // Straight to the next slide — no thinking screen, no agent run.
  await expect(frame.locator('.batch-progress')).toHaveText('Question 2 of 2')
  await expect(frame.locator('.question')).toContainText('Orders live in')
  await frame.locator('.fill-input').fill('SQLite')
  await frame.getByRole('button', { name: 'Submit answer' }).click()

  // The last answer hands the whole batch to ONE record run.
  await expect(frame.getByText('Recording your answers, then the next questions…')).toBeVisible()
  const state = await (
    await request.get('/api/plugin-ui/project-planner/state?repo=.', { headers: scoped })
  ).json()
  expect(state.status).toBe('thinking')
  expect(state.answered).toBe(2)
  expect(state.pending_count).toBe(1)
})
