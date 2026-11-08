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
  await expect(page.getByTestId('setup-final-chat')).toBeVisible()
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
      await expect(page.getByTestId('setup-final-chat')).toBeVisible({ timeout: 10_000 })

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

  test('"Start a chat" finishes setup and opens the new session dialog', async ({
    request,
    page,
  }) => {
    try {
      await passPasswordStep(page, request)
      await skipToReady(page)
      await page.getByTestId('setup-final-chat').click()
      await expect(page.getByTestId('setup-wizard')).toBeHidden({ timeout: 10_000 })
      await expect(page.getByRole('dialog', { name: /session|chat/i })).toBeVisible()
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

  test('"Show me around" finishes setup and lands on Sessions with no dialog', async ({
    request,
    page,
  }) => {
    try {
      await passPasswordStep(page, request)
      await skipToReady(page)
      await page.getByTestId('setup-final-tour').click()
      await expect(page.getByTestId('setup-wizard')).toBeHidden({ timeout: 10_000 })
      // Sessions is the root route.
      await expect(page).toHaveURL(/\/(sessions)?$/)
      await expect(page.locator('.rail-avatar')).toBeVisible()
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
