import { test, expect } from '../harness'
import { spawn, type ChildProcess } from 'node:child_process'
import { existsSync, mkdirSync, mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { SERVER_BIN, freePort, login } from './ssh-harness'

/**
 * The whole first-run path on a genuinely fresh install, as a new user walks
 * it: the 4-step wizard appears on its own, the ready-made folder is used,
 * "Start a chat" lands in a working chat, the getting-started checklist ticks
 * from what just happened, and a project's first card is started from the
 * New Card dialog and picked up by a worker.
 *
 * A private server with an empty HOME (so the ready-made `~/PeckBoard` folder
 * lands in a temp dir) and only the scripted mock provider turned on, so no
 * real AI is involved. The pieces are covered in detail by setup-wizard,
 * provider-setup, onboarding-dialogs and getting-started; this checks they
 * add up to a path with no dead ends.
 */

const ADMIN = 'first-run-admin'
const FIRST_PASS = 'first-password-1234'
const NEW_PASS = 'second-password-5678'

let root: string
let home: string
let base: string
let child: ChildProcess

test.describe.configure({ mode: 'serial' })

test.beforeAll(async () => {
  root = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-first-run-'))
  const dataDir = path.join(root, 'data')
  home = path.join(root, 'home')
  mkdirSync(dataDir)
  mkdirSync(home)
  const port = await freePort()
  base = `http://127.0.0.1:${port}`
  const env: NodeJS.ProcessEnv = {
    ...process.env,
    HOME: home,
    PECKBOARD_DATA_DIR: dataDir,
    PECKBOARD_BOOTSTRAP_USERNAME: ADMIN,
    PECKBOARD_BOOTSTRAP_PASSWORD: FIRST_PASS,
    PECKBOARD_NO_RESUME: '1',
    PECKBOARD_TTS_DOWNLOAD: '0',
    // A fresh install, except that the scripted provider stands in for a
    // signed-in AI.
    PECKBOARD_PREINSTALL_PLUGINS: 'mock,session-control',
  }
  child = spawn(
    SERVER_BIN,
    ['--port', String(port), '--https-port', String(await freePort()), '--host', '127.0.0.1'],
    { env, stdio: 'ignore' },
  )
  const deadline = Date.now() + 30_000
  while (Date.now() < deadline) {
    if (
      await fetch(`${base}/api/health`).then(
        (r) => r.ok,
        () => false,
      )
    )
      return
    await new Promise((r) => setTimeout(r, 200))
  }
  throw new Error('private server never came up')
})

test.afterAll(async () => {
  if (child && child.exitCode === null) {
    const exited = new Promise((r) => child.once('exit', r))
    child.kill('SIGTERM')
    await Promise.race([exited, new Promise((r) => setTimeout(r, 10_000))])
    if (child.exitCode === null) child.kill('SIGKILL')
  }
  if (root) rmSync(root, { recursive: true, force: true })
})

test('a new user goes from first sign-in to a chat and a started card with no dead ends', async ({
  page,
  request,
}) => {
  test.setTimeout(150_000)
  const firstToken = await login(request, base, ADMIN, FIRST_PASS)
  await page.addInitScript((t) => {
    if (!localStorage.getItem('peckboard_token')) localStorage.setItem('peckboard_token', t)
  }, firstToken)
  await page.goto(base)

  // ── 1. Welcome: says what PeckBoard is, then the password ────────
  const wizard = page.getByTestId('setup-wizard')
  await expect(wizard).toBeVisible({ timeout: 10_000 })
  await expect(page.getByTestId('setup-what-is')).toBeVisible()
  await expect(wizard).not.toContainText('stdout')
  await page.getByTestId('setup-pw-current').fill(FIRST_PASS)
  await page.getByTestId('setup-pw-new').fill(NEW_PASS)
  await page.getByTestId('setup-pw-confirm').fill(NEW_PASS)
  await page.getByTestId('setup-next').click()

  // ── 2. Connect your AI: optional, never a trap ───────────────────
  await expect(page.getByTestId('provider-setup')).toBeVisible({ timeout: 15_000 })
  await expect(page.getByTestId('setup-next')).toBeEnabled()
  await page.getByTestId('setup-next').click()

  // ── 3. Where files live: one click, no typed path ────────────────
  const suggested = path.join(home, 'PeckBoard')
  await expect(page.getByTestId('setup-folder-default-path')).toContainText(suggested)
  await page.getByTestId('setup-folder-use-default').click()
  await expect(page.getByTestId('setup-folder-done')).toBeVisible({ timeout: 10_000 })
  expect(existsSync(suggested)).toBe(true)
  await page.getByTestId('setup-next').click()

  // ── 4. Ready: "Start a chat" lands in New Session, ready to go ───
  await expect(page.getByTestId('setup-remote-note')).toBeVisible()
  await page.getByTestId('setup-final-chat').click()
  await expect(wizard).toBeHidden({ timeout: 10_000 })
  await expect(page.locator('#new-session-name')).toBeVisible({ timeout: 10_000 })
  // The wizard's folder is already chosen; only the AI is left to pick.
  await expect(page.locator('#new-session-folder option:checked')).toContainText(suggested)
  await page.getByTestId('new-session-model').click()
  await page.getByTestId('new-session-model-search').fill('echo')
  await page.getByRole('option', { name: 'Mock: echo' }).click()
  await expect(page.getByTestId('new-session-disabled-reason')).toHaveCount(0)
  await page.getByTestId('new-session-create').click()

  // A working chat: the first message gets an answer.
  const input = page.locator('textarea.input-textarea')
  await expect(input).toBeVisible({ timeout: 10_000 })
  await input.fill('hello from a brand new install')
  await input.press('Enter')
  await expect(page.locator('.chat-container')).toContainText('hello from a brand new install', {
    timeout: 20_000,
  })

  const token = await login(request, base, ADMIN, NEW_PASS)
  const auth = { Authorization: `Bearer ${token}` }
  const setup = (await (
    await request.get(`${base}/api/settings/setup`, { headers: auth })
  ).json()) as {
    completed: boolean
  }
  expect(setup.completed).toBe(true)

  // ── The checklist ticks from what just happened ──────────────────
  await page.getByRole('button', { name: 'Sessions', exact: true }).click()
  const checklist = page.getByTestId('getting-started')
  await expect(checklist).toBeVisible({ timeout: 10_000 })
  await expect(checklist.getByTestId('getting-started-folder')).toHaveAttribute('data-done', 'true')
  await expect(checklist.getByTestId('getting-started-session')).toHaveAttribute(
    'data-done',
    'true',
    { timeout: 10_000 },
  )
  await expect(checklist.getByTestId('getting-started-project')).toHaveAttribute(
    'data-done',
    'false',
  )

  // ── First project: Task is preselected, nothing else to decide ───
  await checklist.getByTestId('getting-started-project-action').click()
  await expect(page.getByRole('heading', { name: 'New Project' })).toBeVisible()
  await page.locator('#new-project-name').fill('My first project')
  await expect(page.locator('.workflow-select-trigger')).toContainText('Task')
  await page.getByRole('button', { name: 'Create Project' }).click()
  await expect(page.getByRole('heading', { name: 'New Project' })).toHaveCount(0, {
    timeout: 10_000,
  })

  type Project = { id: string; name: string }
  const listProjects = async () => {
    const body = (await (await request.get(`${base}/api/projects`, { headers: auth })).json()) as
      | Project[]
      | { items: Project[] }
    return Array.isArray(body) ? body : body.items
  }
  let project: Project | undefined
  await expect
    .poll(async () => {
      project = (await listProjects()).find((p) => p.name === 'My first project')
      return project?.id ?? null
    })
    .not.toBeNull()
  // The board worker runs on the scripted provider.
  await request.patch(`${base}/api/projects/${project!.id}`, {
    headers: auth,
    data: { model: 'mock:happy-path' },
  })

  // ── First card: "Add and start" asks about cost, then work begins ─
  await page.goto(`${base}/projects/${project!.id}`)
  await expect(page.getByTestId('kanban-empty')).toContainText('wait in Backlog', {
    timeout: 10_000,
  })
  await page.getByTestId('kanban-empty-add').click()
  const cardModal = page.locator('.modal').filter({ hasText: 'New Card' })
  await expect(cardModal).toBeVisible()
  await cardModal.locator('#card-title').fill('Write a hello file')
  await cardModal.getByTestId('card-form-add-start').click()
  const confirm = page.getByTestId('backlog-start-confirm')
  await expect(confirm).toBeVisible()
  await confirm.getByRole('button', { name: 'Start work' }).click()

  type CardRow = { title: string; step: string }
  await expect
    .poll(
      async () => {
        const res = await request.get(`${base}/api/projects/${project!.id}/cards`, {
          headers: auth,
        })
        const cards = (await res.json()) as CardRow[]
        return cards.find((c) => c.title === 'Write a hello file')?.step ?? null
      },
      { timeout: 15_000 },
    )
    .not.toBe('backlog')

  // Back on Sessions, the project and card steps are done too.
  await page.getByRole('button', { name: 'Sessions', exact: true }).click()
  await expect(checklist.getByTestId('getting-started-project')).toHaveAttribute(
    'data-done',
    'true',
    { timeout: 10_000 },
  )
  await expect(checklist.getByTestId('getting-started-card')).toHaveAttribute('data-done', 'true', {
    timeout: 10_000,
  })
})
