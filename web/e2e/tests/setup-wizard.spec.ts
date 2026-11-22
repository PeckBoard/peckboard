import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E for the first-run setup wizard.
 *
 * The shared e2e server marks setup complete in global-setup (a fresh
 * data dir is a "fresh install", and an incomplete setup would overlay
 * every other spec), so these tests stub `GET /api/settings/setup` to
 * open the wizard, then drive the four steps against the real backend:
 * the password change, default AI, and folder all hit the live API and
 * are asserted through it. Installing and signing in to providers, the
 * Details toggle, the default-AI auto-pick and the ready-made folder are
 * covered on a genuinely fresh install in provider-setup.spec.ts.
 */

const ADMIN_USER = 'e2e-user'
const ADMIN_PASS = 'e2e-password-1234'
const WIZARD_PASS = 'wizard-password-5678'

async function loginToken(
  request: APIRequestContext,
  username: string,
  password: string,
): Promise<string> {
  const res = await request.post('/api/auth/login', { data: { username, password } })
  expect(res.ok(), `login as ${username} failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { token: string }).token
}

/** Serve `{completed:false}` for the setup probe so the wizard mounts. */
async function stubSetupIncomplete(page: Page) {
  await page.route('**/api/settings/setup', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ completed: false }),
    }),
  )
}

/** Plant a token in localStorage and load the SPA. Conditional: after the
 *  wizard's password change the auth store swaps in a fresh token, and an
 *  unconditional init script would overwrite it with the revoked one on
 *  reload. */
async function loadAs(page: Page, token: string) {
  await page.addInitScript((t) => {
    if (!localStorage.getItem('peckboard_token')) localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto('/')
}

/** Put the shared admin password (and default model) back for later specs. */
async function restoreSharedState(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: ADMIN_USER, password: WIZARD_PASS },
  })
  if (res.ok()) {
    const { token } = (await res.json()) as { token: string }
    const change = await request.post('/api/auth/change-password', {
      headers: { Authorization: `Bearer ${token}` },
      data: { current_password: WIZARD_PASS, new_password: ADMIN_PASS },
    })
    expect(change.ok(), `restore password failed: ${await change.text()}`).toBeTruthy()
  }
  const restored = await loginToken(request, ADMIN_USER, ADMIN_PASS)
  const auth = { Authorization: `Bearer ${restored}` }
  await request.put('/api/settings/default-model', { headers: auth, data: { model: '' } })
  await request.put('/api/me/use-cases', { headers: auth, data: { use_cases: [] } })
}

/** Open the wizard and get through the mandatory password step. */
async function passPasswordStep(page: Page, request: APIRequestContext) {
  const adminToken = await loginToken(request, ADMIN_USER, ADMIN_PASS)
  await stubSetupIncomplete(page)
  await loadAs(page, adminToken)
  await expect(page.getByTestId('setup-wizard')).toBeVisible({ timeout: 10_000 })
  await page.getByTestId('setup-pw-current').fill(ADMIN_PASS)
  await page.getByTestId('setup-pw-new').fill(WIZARD_PASS)
  await page.getByTestId('setup-pw-confirm').fill(WIZARD_PASS)
  await page.getByTestId('setup-next').click()
  await expect(page.getByTestId('provider-setup')).toBeVisible({ timeout: 15_000 })
}

/** From the providers step, skip to the last step without adding anything. */
async function skipToReady(page: Page) {
  await page.getByTestId('setup-next').click()
  await expect(page.getByRole('heading', { name: /Where files live/ })).toBeVisible()
  await page.getByTestId('setup-next').click()
  await expect(page.getByTestId('setup-final-tour')).toBeVisible()
}

async function expectSetupCompleted(request: APIRequestContext) {
  const token = await loginToken(request, ADMIN_USER, WIZARD_PASS)
  const res = await request.get('/api/settings/setup', {
    headers: { Authorization: `Bearer ${token}` },
  })
  expect((await res.json()).completed, 'setup must be marked complete').toBe(true)
}

test.describe('first-run setup wizard', () => {
  test('walks all four steps: welcome + password, AI, folder typed then saved by Next, ready', async ({
    request,
    page,
  }) => {
    const adminToken = await loginToken(request, ADMIN_USER, ADMIN_PASS)

    try {
      await stubSetupIncomplete(page)
      await loadAs(page, adminToken)

      const wizard = page.getByTestId('setup-wizard')
      await expect(wizard).toBeVisible({ timeout: 10_000 })

      // Progress labels match the step headings.
      await expect(page.getByTestId('setup-progress')).toContainText('Welcome')
      await expect(page.getByTestId('setup-progress')).toContainText('Connect your AI')
      await expect(page.getByTestId('setup-progress')).toContainText('Where files live')
      await expect(page.getByTestId('setup-progress')).toContainText('Ready')
      // Optional use-case chips: each toggle is saved for this user.
      await expect(page.getByTestId('setup-usecases')).toContainText(
        'What will you use PeckBoard for?',
      )
      const useCasesSaved = page.waitForRequest(
        (r) =>
          r.url().endsWith('/api/me/use-cases') &&
          r.method() === 'PUT' &&
          r.postDataJSON()?.use_cases?.length === 2,
      )
      await page.getByTestId('setup-usecase-coding').click()
      await page.getByTestId('setup-usecase-printing').click()
      await expect(page.getByTestId('setup-usecase-coding')).toHaveAttribute('aria-pressed', 'true')
      await expect(page.getByTestId('setup-usecase-writing')).toHaveAttribute(
        'aria-pressed',
        'false',
      )
      await useCasesSaved

      // ── Step 1: what PeckBoard is, then the mandatory password ─────
      await expect(page.getByTestId('setup-what-is')).toContainText('AI helpers')
      await expect(wizard).not.toContainText('stdout')
      await expect(page.getByLabel('Password you just signed in with')).toBeVisible()
      const next = page.getByTestId('setup-next')
      await expect(next).toBeDisabled()

      // Too-short password keeps the gate shut.
      await page.getByTestId('setup-pw-current').fill(ADMIN_PASS)
      await page.getByTestId('setup-pw-new').fill('short')
      await page.getByTestId('setup-pw-confirm').fill('short')
      await expect(next).toBeDisabled()

      await page.getByTestId('setup-pw-new').fill(WIZARD_PASS)
      await page.getByTestId('setup-pw-confirm').fill(WIZARD_PASS)
      await expect(next).toBeEnabled()
      await next.click()

      // Advanced to providers; the change really landed (old password dead).
      await expect(page.getByTestId('provider-setup')).toBeVisible({ timeout: 15_000 })
      const oldLogin = await request.post('/api/auth/login', {
        data: { username: ADMIN_USER, password: ADMIN_PASS },
      })
      expect(oldLogin.status()).toBe(401)

      // ── Step 2: plans, advice, and the default AI line ─────────────
      const freshToken = await loginToken(request, ADMIN_USER, WIZARD_PASS)
      const auth = { Authorization: `Bearer ${freshToken}` }
      await expect(page.getByTestId('provider-setup-plan-claude')).toHaveText(
        'Uses Claude Pro or Max',
      )
      await expect(page.getByTestId('provider-setup-plan-codex')).toHaveText(
        'Uses ChatGPT Plus or Pro',
      )
      await expect(page.getByTestId('provider-setup-advice')).toContainText(
        'Not sure? Claude or ChatGPT.',
      )
      await expect(page.getByTestId('provider-setup-more')).toHaveText(
        'Self-hosted models (advanced)',
      )
      // The wizard never shows Settings' hide switches.
      await expect(page.locator('[data-testid^="provider-toggle-"]')).toHaveCount(0)

      // Default AI: the mock catalogue is there, so the line shows and its
      // "change" reveals the searchable model picker.
      await expect(page.getByTestId('setup-default-ai')).toBeVisible()
      await page.getByTestId('setup-default-ai-change').click()
      await page.getByTestId('setup-default-model').click()
      const search = page.getByTestId('setup-default-model-search')
      await search.fill('happy')
      await expect(page.getByTestId('setup-default-model-option-mock:echo')).toBeHidden()
      await page.getByTestId('setup-default-model-option-mock:happy-path').click()
      await expect(page.getByTestId('setup-default-ai-name')).toHaveText('Mock: happy path')
      await expect
        .poll(async () => {
          const res = await request.get('/api/settings/default-model', { headers: auth })
          return ((await res.json()) as { model?: string }).model
        })
        .toBe('mock:happy-path')

      await page.getByTestId('setup-next').click()

      // ── Step 3: typed fields are saved by Next, or Next says why not ─
      await expect(page.getByRole('heading', { name: /Where files live/ })).toBeVisible()
      await expect(wizard).toContainText('The AI can read and change files in this folder.')
      await page.getByTestId('setup-folder-choose-other').click()

      const folderPath = path.join(
        mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-wizard-')),
        'not-yet-made',
      )
      const folderName = `setup-wiz-${Date.now()}`

      // Half-filled: Next blocks and names the missing piece.
      await page.getByTestId('setup-folder-name').fill(folderName)
      await page.getByTestId('setup-next').click()
      await expect(page.getByTestId('setup-folder-error')).toContainText('Enter where the folder')
      await expect(page.getByTestId('setup-folder-name')).toHaveValue(folderName)

      // Fully filled: Next alone saves it (no "Add folder" click).
      await page.getByTestId('setup-folder-path').fill(folderPath)
      await page.getByTestId('setup-next').click()
      // AI (the default model above) and folder both ready: "Try it now" is on.
      await expect(page.getByTestId('setup-final-try')).toBeEnabled({ timeout: 10_000 })
      await expect(page.getByTestId('setup-final-chat')).toHaveCount(0)

      const foldersRes = await request.get('/api/folders', { headers: auth })
      const folders = (await foldersRes.json()) as { name: string; path: string }[]
      expect(
        folders.find((f) => f.name === folderName)?.path,
        'Next must save the typed folder',
      ).toBe(folderPath)

      // ── Step 4: the HTTPS admin is gone, a pointer stays ───────────
      await expect(page.getByTestId('tls-status-https')).toHaveCount(0)
      await expect(page.getByTestId('setup-remote-note')).toContainText('Secure connection')

      // Back shows the folder as done.
      await page.getByTestId('setup-back').click()
      await expect(page.getByTestId('setup-folder-done')).toBeVisible()
      await page.getByTestId('setup-next').click()

      await page.getByTestId('setup-finish').click()
      await expect(wizard).toBeHidden({ timeout: 10_000 })
      // The use-case chips were saved for this user.
      const useCasesRes = await request.get('/api/me/use-cases', { headers: auth })
      expect(((await useCasesRes.json()) as { use_cases: string[] }).use_cases).toEqual([
        'coding',
        'printing',
      ])
      await expectSetupCompleted(request)

      // With the stub gone the real (completed) state keeps it hidden.
      await page.unroute('**/api/settings/setup')
      await page.reload()
      await expect(page.locator('.rail-avatar')).toBeVisible({ timeout: 10_000 })
      await expect(page.getByTestId('setup-wizard')).toHaveCount(0)
    } finally {
      await restoreSharedState(request)
    }
  })
  test('with no AI ready, "Try it now" says why and "Start a chat" opens the new session dialog', async ({
    request,
    page,
  }) => {
    try {
      // No AI tool ready and no default model, whatever this host has.
      await page.route('**/api/provider-setup', (route) =>
        route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify({ providers: [], other_providers: [] }),
        }),
      )
      await page.route('**/api/settings/default-model', (route) =>
        route.request().method() === 'GET'
          ? route.fulfill({
              status: 200,
              contentType: 'application/json',
              body: JSON.stringify({ model: '' }),
            })
          : route.continue(),
      )
      await passPasswordStep(page, request)
      await skipToReady(page)
      await expect(page.getByTestId('setup-final-try')).toBeDisabled()
      await expect(page.getByTestId('setup-final-try-reason')).toContainText('connected AI')
      await page.getByTestId('setup-final-chat').click()
      await expect(page.getByTestId('setup-wizard')).toBeHidden({ timeout: 10_000 })
      await expect(page.getByRole('dialog', { name: /session|chat/i })).toBeVisible()
      await expectSetupCompleted(request)
    } finally {
      await restoreSharedState(request)
    }
  })

  test('"Try it now" finishes setup and opens a chat already looking around the folder', async ({
    request,
    page,
  }) => {
    try {
      const token = await loginToken(request, ADMIN_USER, ADMIN_PASS)
      const auth = { Authorization: `Bearer ${token}` }
      // An AI (the default model) and a folder are what it needs.
      const model = await request.put('/api/settings/default-model', {
        headers: auth,
        data: { model: 'mock:echo' },
      })
      expect(model.ok(), `set default model failed: ${await model.text()}`).toBeTruthy()
      const created = await request.post('/api/folders', {
        headers: auth,
        data: {
          name: `try-it-${Date.now()}`,
          path: mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-try-')),
        },
      })
      expect(created.ok(), `create folder failed: ${await created.text()}`).toBeTruthy()

      await passPasswordStep(page, request)
      await skipToReady(page)
      const tryNow = page.getByTestId('setup-final-try')
      await expect(tryNow).toBeEnabled({ timeout: 10_000 })
      await expect(tryNow).toContainText('Watch an AI helper look around your folder.')
      await expect(page.getByTestId('setup-final-try-reason')).toHaveCount(0)
      await expect(page.getByTestId('setup-final-chat')).toHaveCount(0)
      await tryNow.click()

      await expect(page.getByTestId('setup-wizard')).toBeHidden({ timeout: 10_000 })
      await expect(page).toHaveURL(/\/sessions\/[^/]+/)
      // The first message was sent for the user, and the echo model answered.
      await expect(page.locator('.chat-container')).toContainText(
        'Look around this folder and tell me',
        { timeout: 20_000 },
      )
      await expectSetupCompleted(request)
    } finally {
      await restoreSharedState(request)
    }
  })

  test('"Create a project" finishes setup and opens the new project dialog', async ({
    request,
    page,
  }) => {
    try {
      await passPasswordStep(page, request)
      await skipToReady(page)
      await page.getByTestId('setup-final-project').click()
      await expect(page.getByTestId('setup-wizard')).toBeHidden({ timeout: 10_000 })
      await expect(page.getByRole('dialog', { name: /project/i })).toBeVisible()
      await expectSetupCompleted(request)
    } finally {
      await restoreSharedState(request)
    }
  })

  test('"Show me around" finishes setup and opens the Welcome page', async ({ request, page }) => {
    try {
      await passPasswordStep(page, request)
      await skipToReady(page)
      await page.getByTestId('setup-final-tour').click()
      await expect(page.getByTestId('setup-wizard')).toBeHidden({ timeout: 10_000 })
      await expect(page).toHaveURL(/\/welcome$/)
      await expect(page.getByTestId('welcome-page')).toBeVisible()
      await expect(page.getByRole('dialog', { name: /session|project|chat/i })).toHaveCount(0)
      await expectSetupCompleted(request)
    } finally {
      await restoreSharedState(request)
    }
  })

  test('a non-admin never sees the wizard even while setup is incomplete', async ({
    request,
    page,
  }) => {
    const adminToken = await loginToken(request, ADMIN_USER, ADMIN_PASS)
    const username = `wiz-nonadmin-${Date.now()}`
    const password = 'nonadmin-password-1234'
    const create = await request.post('/api/users', {
      headers: { Authorization: `Bearer ${adminToken}` },
      data: { username, password, role: 'user' },
    })
    expect(create.ok(), `create user failed: ${await create.text()}`).toBeTruthy()

    await stubSetupIncomplete(page)
    const token = await loginToken(request, username, password)
    await loadAs(page, token)
    await expect(page.locator('.rail-avatar')).toBeVisible({ timeout: 10_000 })
    await expect(page.getByTestId('setup-wizard')).toHaveCount(0)
  })
})

/**
 * Resume: the wizard reads the stored progress (`password_changed`, the
 * furthest `step`) and opens on the first step that is neither done nor
 * already passed, instead of asking for the temporary password again.
 * The server side (recording both) is covered in `routes/settings.rs`.
 */
test.describe('setup wizard resumes', () => {
  async function openWithProgress(
    page: Page,
    request: APIRequestContext,
    progress: { password_changed: boolean; step: number },
  ) {
    await page.route('**/api/settings/setup', (route) =>
      route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({ completed: false, ...progress }),
      }),
    )
    // No AI tool ready, whatever CLIs this host happens to have, so only the
    // stored progress decides where the wizard opens.
    await page.route('**/api/provider-setup', (route) =>
      route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({ providers: [], other_providers: [] }),
      }),
    )
    await loadAs(page, await loginToken(request, ADMIN_USER, ADMIN_PASS))
    await expect(page.getByTestId('setup-wizard')).toBeVisible({ timeout: 10_000 })
  }

  test('a changed password skips step 1, and Next records the step reached', async ({
    request,
    page,
  }) => {
    await openWithProgress(page, request, { password_changed: true, step: 0 })

    await expect(page.getByTestId('provider-setup')).toBeVisible({ timeout: 15_000 })
    await expect(page.getByTestId('setup-pw-current')).toHaveCount(0)
    await expect(page.getByTestId('setup-progress-step-2')).toHaveAttribute('aria-current', 'step')
    await expect(page.getByTestId('setup-progress-step-1')).toContainText('✓')

    // Back shows step 1 as done, with no password fields to fill again.
    await page.getByTestId('setup-back').click()
    await expect(page.getByTestId('setup-pw-done')).toBeVisible()
    await page.getByTestId('setup-next').click()

    const saved = page.waitForRequest(
      (r) =>
        r.url().endsWith('/api/settings/setup/progress') &&
        r.method() === 'PUT' &&
        r.postDataJSON()?.step === 2,
    )
    await page.getByTestId('setup-next').click()
    expect((await saved).postDataJSON()).toEqual({ step: 2 })
    await expect(page.getByRole('heading', { name: /Where files live/ })).toBeVisible()
  })

  test('a stored step reopens there, past steps that were skipped', async ({ request, page }) => {
    await openWithProgress(page, request, { password_changed: true, step: 3 })
    await expect(page.getByTestId('setup-final-tour')).toBeVisible({ timeout: 15_000 })
    await expect(page.getByTestId('setup-progress-step-4')).toHaveAttribute('aria-current', 'step')
  })
})
