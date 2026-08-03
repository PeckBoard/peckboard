import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E for the first-run setup wizard.
 *
 * The shared e2e server marks setup complete in global-setup (a fresh
 * data dir is a "fresh install", and an incomplete setup would overlay
 * every other spec), so these tests stub `GET /api/settings/setup` to
 * open the wizard, then drive the five steps against the real backend:
 * the password change, provider toggles, default model, and folder all
 * hit the live API and are asserted through it.
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

test.describe('first-run setup wizard', () => {
  test('walks all five steps: password gate, providers, model, folder, TLS, finish', async ({
    request,
    page,
  }) => {
    const adminToken = await loginToken(request, ADMIN_USER, ADMIN_PASS)
    let passwordChanged = false

    try {
      await stubSetupIncomplete(page)
      await loadAs(page, adminToken)

      const wizard = page.getByTestId('setup-wizard')
      await expect(wizard).toBeVisible({ timeout: 10_000 })

      // ── Step 1: password is mandatory ─────────────────────────────
      const next = page.getByTestId('setup-next')
      await expect(next).toBeDisabled()
      await expect(wizard).toContainText('printed its password')

      // Too-short password keeps the gate shut.
      await page.getByTestId('setup-pw-current').fill(ADMIN_PASS)
      await page.getByTestId('setup-pw-new').fill('short')
      await page.getByTestId('setup-pw-confirm').fill('short')
      await expect(next).toBeDisabled()

      await page.getByTestId('setup-pw-new').fill(WIZARD_PASS)
      await page.getByTestId('setup-pw-confirm').fill(WIZARD_PASS)
      await expect(next).toBeEnabled()
      await next.click()
      passwordChanged = true

      // Advanced to providers; the change really landed (old password dead).
      await expect(page.getByTestId('setup-provider-toggle-claude')).toBeVisible({
        timeout: 10_000,
      })
      const oldLogin = await request.post('/api/auth/login', {
        data: { username: ADMIN_USER, password: ADMIN_PASS },
      })
      expect(oldLogin.status()).toBe(401)

      // ── Step 2: hide a provider, verify it persisted ───────────────
      const claudeToggle = page.getByTestId('setup-provider-toggle-claude')
      await expect(claudeToggle).toBeChecked()
      await claudeToggle.uncheck()

      const freshToken = await loginToken(request, ADMIN_USER, WIZARD_PASS)
      const auth = { Authorization: `Bearer ${freshToken}` }
      await expect
        .poll(async () => {
          const res = await request.get('/api/settings/providers', { headers: auth })
          const data = (await res.json()) as { providers: { id: string; hidden: boolean }[] }
          return data.providers.find((p) => p.id === 'claude')?.hidden
        })
        .toBe(true)

      await page.getByTestId('setup-next').click()

      // ── Step 3: searchable model picker, filtered to enabled providers ─
      await page.getByTestId('setup-default-model').click()
      const search = page.getByTestId('setup-default-model-search')
      await expect(search).toBeVisible()

      // Claude is hidden, so no claude-prefixed options may be offered.
      await expect(page.getByTestId('setup-default-model-option-mock:echo')).toBeVisible()
      await expect(page.locator('[data-testid^="setup-default-model-option-claude:"]')).toHaveCount(
        0,
      )

      // Typing narrows the list (combobox, not a plain dropdown).
      await search.fill('happy')
      await expect(page.getByTestId('setup-default-model-option-mock:happy-path')).toBeVisible()
      await expect(page.getByTestId('setup-default-model-option-mock:echo')).toBeHidden()
      await page.getByTestId('setup-default-model-option-mock:happy-path').click()

      await expect
        .poll(async () => {
          const res = await request.get('/api/settings/default-model', { headers: auth })
          return ((await res.json()) as { model?: string }).model
        })
        .toBe('mock:happy-path')

      await page.getByTestId('setup-next').click()

      // ── Step 4: register a folder ──────────────────────────────────
      // The folder step is skippable: Next stays enabled with nothing filled in.
      await expect(page.getByTestId('setup-next')).toBeEnabled()

      const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-wizard-'))
      const folderName = `setup-wiz-${Date.now()}`
      await page.getByTestId('setup-folder-name').fill(folderName)
      await page.getByTestId('setup-folder-path').fill(folderPath)
      await page.getByTestId('setup-folder-add').click()
      await expect(page.getByTestId('setup-folder-done')).toContainText(folderName)

      const foldersRes = await request.get('/api/folders', { headers: auth })
      const folders = (await foldersRes.json()) as { name: string }[]
      expect(folders.some((f) => f.name === folderName)).toBeTruthy()

      await page.getByTestId('setup-next').click()

      // ── Step 5: TLS review + finish ────────────────────────────────
      await expect(page.getByTestId('tls-status-https')).toBeVisible({ timeout: 10_000 })
      await expect(wizard).toContainText('re-run any time from Settings')

      // Back navigation keeps earlier answers (step 4 still shows the folder).
      await page.getByTestId('setup-back').click()
      await expect(page.getByTestId('setup-folder-done')).toContainText(folderName)
      await page.getByTestId('setup-next').click()

      await page.getByTestId('setup-finish').click()
      await expect(wizard).toBeHidden({ timeout: 10_000 })

      const setupRes = await request.get('/api/settings/setup', { headers: auth })
      expect((await setupRes.json()).completed, 'setup must be marked complete after finish').toBe(
        true,
      )

      // With the stub gone the real (completed) state keeps it hidden.
      await page.unroute('**/api/settings/setup')
      await page.reload()
      await expect(page.locator('.rail-avatar')).toBeVisible({ timeout: 10_000 })
      await expect(page.getByTestId('setup-wizard')).toHaveCount(0)
    } finally {
      // Restore shared state for the rest of the suite: bootstrap
      // password, provider visibility, and the app default model.
      if (passwordChanged) {
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
      }
      const restored = await loginToken(request, ADMIN_USER, ADMIN_PASS)
      const auth = { Authorization: `Bearer ${restored}` }
      await request.put('/api/settings/providers/claude', {
        headers: auth,
        data: { hidden: false },
      })
      await request.put('/api/settings/default-model', { headers: auth, data: { model: '' } })
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
