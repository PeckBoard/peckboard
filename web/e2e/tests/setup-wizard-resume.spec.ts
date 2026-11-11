import { test, expect, type Page } from '../harness'
import { spawn, type ChildProcess } from 'node:child_process'
import { mkdirSync, mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { SERVER_BIN, freePort, login } from './ssh-harness'

/**
 * The setup wizard resumes at every step, on a genuinely fresh install with
 * nothing stubbed: a private server, the real setup state, real folders.
 * After each step the page is reloaded (and once, a brand-new login) and the
 * wizard must reopen on the first step that is neither done nor passed:
 *
 *   1 Welcome + password → stays until the password is changed
 *   2 Connect your AI    → a skipped step stays skipped
 *   3 Where files live   → a folder added elsewhere counts as done
 *   4 Ready              → Back never rewinds the resume point
 *   finished             → the wizard never comes back
 */

const ADMIN = 'resume-admin'
const FIRST_PASS = 'first-password-1234'
const NEW_PASS = 'second-password-5678'

let root: string
let base: string
let child: ChildProcess

test.describe.configure({ mode: 'serial' })

test.beforeAll(async () => {
  root = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-resume-'))
  const dataDir = path.join(root, 'data')
  const home = path.join(root, 'home')
  for (const d of [dataDir, home]) mkdirSync(d)
  const port = await freePort()
  base = `http://127.0.0.1:${port}`
  const env: NodeJS.ProcessEnv = {
    ...process.env,
    HOME: home,
    PATH: '/usr/bin:/bin',
    PECKBOARD_DATA_DIR: dataDir,
    PECKBOARD_BOOTSTRAP_USERNAME: ADMIN,
    PECKBOARD_BOOTSTRAP_PASSWORD: FIRST_PASS,
    PECKBOARD_NO_RESUME: '1',
    PECKBOARD_TTS_DOWNLOAD: '0',
  }
  delete env.PECKBOARD_PREINSTALL_PLUGINS
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

async function open(page: Page, token: string) {
  await page.addInitScript((t) => {
    if (!localStorage.getItem('peckboard_token')) localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(base)
}

/** The wizard is up, done loading, and on step `n` (1-based). */
async function expectOnStep(page: Page, n: number) {
  await expect(page.getByTestId('setup-wizard')).toBeVisible({ timeout: 15_000 })
  await expect(page.getByTestId(`setup-progress-step-${n}`)).toHaveAttribute(
    'aria-current',
    'step',
    { timeout: 15_000 },
  )
}

async function reloadOnStep(page: Page, n: number) {
  await page.reload()
  await expectOnStep(page, n)
}

test('the wizard resumes at every step and never comes back once finished', async ({
  page,
  browser,
  request,
}) => {
  const firstToken = await login(request, base, ADMIN, FIRST_PASS)
  await open(page, firstToken)

  // Step 1: nothing done yet, so a reload stays on the password step.
  await expectOnStep(page, 1)
  await expect(page.getByTestId('setup-pw-current')).toBeVisible()
  await reloadOnStep(page, 1)
  await expect(page.getByTestId('setup-pw-current')).toBeVisible()

  await page.getByTestId('setup-pw-current').fill(FIRST_PASS)
  await page.getByTestId('setup-pw-new').fill(NEW_PASS)
  await page.getByTestId('setup-pw-confirm').fill(NEW_PASS)
  await page.getByTestId('setup-next').click()
  await expectOnStep(page, 2)

  // Step 2: the password change is remembered, on a reload and on a brand-new
  // login, and step 1 is ticked rather than asking again.
  await reloadOnStep(page, 2)
  await expect(page.getByTestId('setup-progress-step-1')).toContainText('✓')
  await expect(page.getByTestId('setup-pw-current')).toHaveCount(0)
  const fresh = await browser.newPage()
  await open(fresh, await login(request, base, ADMIN, NEW_PASS))
  await expectOnStep(fresh, 2)
  await fresh.close()

  // Skipping the AI step: it stays skipped after a reload.
  await page.getByTestId('setup-next').click()
  await expectOnStep(page, 3)
  await expect(page.getByRole('heading', { name: /Where files live/ })).toBeVisible()
  await reloadOnStep(page, 3)

  // Step 3 done outside the wizard (a folder added via the API): it is
  // skipped on the next open, and Back shows it as done.
  const token = await login(request, base, ADMIN, NEW_PASS)
  const folderPath = path.join(root, 'work')
  const created = await request.post(`${base}/api/folders`, {
    headers: { Authorization: `Bearer ${token}` },
    data: { name: 'Work', path: folderPath, create: true },
  })
  expect(created.ok(), `create folder failed: ${await created.text()}`).toBeTruthy()
  await reloadOnStep(page, 4)
  await expect(page.getByTestId('setup-final-chat')).toBeVisible()
  await expect(page.getByTestId('setup-progress-step-3')).toContainText('✓')
  await page.getByTestId('setup-back').click()
  await expectOnStep(page, 3)
  await expect(page.getByTestId('setup-folder-done')).toContainText('Work')

  // Step 4: going Back never rewinds where the wizard resumes.
  await reloadOnStep(page, 4)
  const setup = async () =>
    (await (
      await request.get(`${base}/api/settings/setup`, {
        headers: { Authorization: `Bearer ${token}` },
      })
    ).json()) as { completed: boolean; password_changed: boolean; step: number }
  expect(await setup()).toMatchObject({ completed: false, password_changed: true })
  expect((await setup()).step).toBeGreaterThanOrEqual(2)

  // Finished: it never comes back.
  await page.getByTestId('setup-final-tour').click()
  await expect(page.getByTestId('setup-wizard')).toBeHidden({ timeout: 10_000 })
  await expect.poll(async () => (await setup()).completed).toBe(true)
  await page.reload()
  await expect(page.locator('.rail-avatar')).toBeVisible({ timeout: 15_000 })
  await expect(page.getByTestId('setup-wizard')).toHaveCount(0)
})
