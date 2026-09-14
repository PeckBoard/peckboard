import { test, expect, type APIRequestContext, type Page } from '../harness'
import { chmodSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

/**
 * UI e2e for multi-account Codex ChatGPT sign-in (Settings → Codex Accounts).
 *
 * Two flows, mirroring kimi-accounts.spec.ts but ChatGPT-only (no API-key kind):
 *  1. Add / list / picker / delete a device account. Sign-in is closed without
 *     completing so the row stays "Not signed in"; the catalogue still lists
 *     the account-scoped seed models.
 *  2. The ChatGPT device sign-in flow: add an account, then confirm the
 *     sign-in modal surfaces the `…/codex/device` link and one-time code.
 *     The real login spawns the `codex` CLI, so `login/start` is stubbed;
 *     the URL scraper + login manager are unit-tested separately.
 *  3. The same flow with the real `login/start` route, against a fake `codex`
 *     binary that reprints codex 0.153.4's device prompt verbatim — end-to-end
 *     coverage of spawn → scrape → modal that a stubbed route cannot give.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

async function loadApp(page: Page, token: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/')
  await expect(page.locator('.rail-brand')).toBeVisible({ timeout: 10_000 })
}

async function openSettings(page: Page) {
  await page.locator('.rail-avatar').click()
  const menu = page.locator('.user-menu-dropdown')
  await expect(menu).toBeVisible()
  await menu.getByRole('menuitem', { name: 'Settings' }).click()
  const settings = page.getByTestId('settings-page')
  await expect(settings).toBeVisible()
  await settings.getByTestId('settings-nav-providers').click()
  return settings
}

/** Account-scoped model display names served by `/api/models`. */
async function accountModelLabels(request: APIRequestContext, token: string): Promise<string[]> {
  const res = await request.get('/api/models', {
    headers: { Authorization: `Bearer ${token}` },
  })
  expect(res.ok()).toBeTruthy()
  const body = (await res.json()) as { models: { id: string; display_name: string }[] }
  return body.models.map((m) => m.display_name)
}

function hasCodexAccountModel(labels: string[]): boolean {
  return labels.some((n) => n.startsWith('[E2E Codex]'))
}

test('add, list, expose-in-picker, and delete a Codex ChatGPT account', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await loadApp(page, token)

  expect(hasCodexAccountModel(await accountModelLabels(request, token))).toBe(false)

  const settings = await openSettings(page)
  const section = settings.getByTestId('codex-accounts-section')
  await expect(section).toBeVisible()
  await expect(section).toContainText('No accounts added yet')

  await section.getByTestId('codex-acct-add').click()
  const modal = page.getByTestId('codex-account-modal')
  await expect(modal).toBeVisible()

  await modal.getByTestId('codex-acct-name').fill('E2E Codex')
  await modal.getByTestId('codex-acct-window').selectOption('24')
  await modal.getByTestId('codex-acct-limit-tokens').fill('1000000')
  await modal.getByTestId('codex-acct-save').click()
  await expect(modal).toBeHidden()

  // Creating a ChatGPT account opens sign-in; close it without completing.
  const signIn = page.getByTestId('codex-signin-modal')
  await expect(signIn).toBeVisible()
  await signIn.getByTestId('codex-signin-close').click()

  const row = section.locator('[data-testid^="codex-acct-row-"]')
  await expect(row).toHaveCount(1)
  await expect(row).toContainText('E2E Codex')
  await expect(row).toContainText('ChatGPT')
  await expect(section.locator('[data-testid^="codex-acct-unauth-"]')).toBeVisible()

  await expect
    .poll(async () => hasCodexAccountModel(await accountModelLabels(request, token)))
    .toBe(true)

  await row.locator('[data-testid^="codex-acct-delete-"]').click()
  const confirm = page.locator('.confirm-dialog')
  await expect(confirm).toBeVisible()
  await confirm.getByRole('button', { name: 'Delete' }).click()

  await expect(section.locator('[data-testid^="codex-acct-row-"]')).toHaveCount(0)
  await expect
    .poll(async () => hasCodexAccountModel(await accountModelLabels(request, token)))
    .toBe(false)
})

test('device sign-in flow: add account then surface the ChatGPT device link', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await loadApp(page, token)

  await page.route('**/api/codex-accounts/*/login/start', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        url: 'https://auth.openai.com/codex/device',
        user_code: 'ABCD-EFGH',
      }),
    })
  })

  const settings = await openSettings(page)
  const section = settings.getByTestId('codex-accounts-section')
  await section.getByTestId('codex-acct-add').click()
  const modal = page.getByTestId('codex-account-modal')
  await expect(modal).toBeVisible()

  await modal.getByTestId('codex-acct-name').fill('E2E Codex Sub')
  await modal.getByTestId('codex-acct-save').click()
  await expect(modal).toBeHidden()

  const signIn = page.getByTestId('codex-signin-modal')
  await expect(signIn).toBeVisible()
  await signIn.getByTestId('codex-signin-start').click()
  const link = signIn.getByTestId('codex-signin-url')
  await expect(link).toBeVisible()
  await expect(link).toHaveAttribute('href', /\/codex\/device/)
  await expect(signIn.getByTestId('codex-signin-code')).toContainText('ABCD-EFGH')
  await expect(signIn.getByTestId('codex-signin-waiting')).toBeVisible()

  await signIn.getByTestId('codex-signin-close').click()

  const row = section.locator('[data-testid^="codex-acct-row-"]')
  await expect(row).toContainText('E2E Codex Sub')
  await expect(section.locator('[data-testid^="codex-acct-unauth-"]')).toBeVisible()
})

/**
 * Verbatim `codex login --device-auth` prompt from codex 0.153.4, ANSI and
 * all. The one-time code is `XXXX-XXXXX` (4-5); an earlier scraper only
 * matched 4-4 codes and every sign-in died on "timed out waiting for `codex
 * login --device-auth` to produce a ChatGPT sign-in URL".
 */
const FAKE_CODEX = `#!/bin/sh
printf '\\nWelcome to Codex [v\\033[90m0.153.4\\033[0m]\\n'
printf '\\033[90mOpenAI'"'"'s command-line coding agent\\033[0m\\n\\n'
printf 'Follow these steps to sign in with ChatGPT using device code authorization:\\n\\n'
printf '1. Open this link in your browser and sign in to your account\\n'
printf '   \\033[94mhttps://auth.openai.com/codex/device\\033[0m\\n\\n'
printf '2. Enter this one-time code \\033[90m(expires in 15 minutes)\\033[0m\\n'
printf '   \\033[94m4UWK-LDLPZ\\033[0m\\n'
# Real codex blocks here polling OpenAI; exit before the suite does.
sleep 20
`

test('device sign-in flow: real login/start scrapes the codex CLI prompt', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)

  const fakeCli = join(tmpdir(), `peckboard-fake-codex-${process.pid}-${Date.now()}.sh`)
  writeFileSync(fakeCli, FAKE_CODEX)
  chmodSync(fakeCli, 0o755)

  const put = await request.put('/api/plugins/codex/settings', {
    headers: { Authorization: `Bearer ${token}` },
    data: { updates: { cli_path: fakeCli } },
  })
  expect(put.ok(), `setting cli_path failed: ${await put.text()}`).toBeTruthy()

  await loadApp(page, token)
  const settings = await openSettings(page)
  const section = settings.getByTestId('codex-accounts-section')
  await section.getByTestId('codex-acct-add').click()
  const modal = page.getByTestId('codex-account-modal')
  await expect(modal).toBeVisible()
  await modal.getByTestId('codex-acct-name').fill('E2E Codex CLI')
  await modal.getByTestId('codex-acct-save').click()
  await expect(modal).toBeHidden()

  // No route stub: this hits the server, which spawns the fake CLI and
  // scrapes its output.
  const signIn = page.getByTestId('codex-signin-modal')
  await expect(signIn).toBeVisible()
  await signIn.getByTestId('codex-signin-start').click()

  const link = signIn.getByTestId('codex-signin-url')
  await expect(link).toBeVisible({ timeout: 15_000 })
  await expect(link).toHaveAttribute('href', 'https://auth.openai.com/codex/device')
  await expect(signIn.getByTestId('codex-signin-code')).toContainText('4UWK-LDLPZ')

  await signIn.getByTestId('codex-signin-close').click()
  // Deleting the account cancels the spawned login process. Other specs share
  // this server, so scope the row by name rather than "the only row".
  const row = section
    .locator('[data-testid^="codex-acct-row-"]')
    .filter({ hasText: 'E2E Codex CLI' })
  await row.locator('[data-testid^="codex-acct-delete-"]').click()
  const confirm = page.locator('.confirm-dialog')
  await expect(confirm).toBeVisible()
  await confirm.getByRole('button', { name: 'Delete' }).click()
  await expect(row).toHaveCount(0)
})

/**
 * One fake CLI that behaves differently across its two runs against the same
 * CODEX_HOME. First run: writes credentials immediately, so the account reads
 * as authenticated (a non-empty `auth.json` is all the server checks) and the
 * row offers "Re-sign in". Second run: prints a *different* one-time code and
 * writes nothing — so "still signed in" afterwards can only mean the stale
 * credentials survived the re-login.
 */
const FAKE_CODEX_TWO_RUNS = `#!/bin/sh
printf '   https://auth.openai.com/codex/device\\n'
if [ -f "$CODEX_HOME/.ran-once" ]; then
  printf '   9ZZZ-QQQQQ\\n'
else
  : > "$CODEX_HOME/.ran-once"
  printf '{"tokens":{"fake":true}}\\n' > "$CODEX_HOME/auth.json"
  printf '   4UWK-LDLPZ\\n'
fi
sleep 20
`

/**
 * Point the codex plugin at a fake CLI. `discover_models: false` matters:
 * model discovery spawns the same binary with each account's CODEX_HOME, and
 * a fake that changes behaviour per run would otherwise burn its first run on
 * discovery instead of on the login under test.
 */
async function useFakeCodexCli(request: APIRequestContext, token: string, cliPath: string) {
  const put = await request.put('/api/plugins/codex/settings', {
    headers: { Authorization: `Bearer ${token}` },
    data: { updates: { cli_path: cliPath, discover_models: false } },
  })
  expect(put.ok(), `setting cli_path failed: ${await put.text()}`).toBeTruthy()
}

function writeFakeCli(tag: string, body: string): string {
  const path = join(tmpdir(), `peckboard-fake-codex-${tag}-${process.pid}-${Date.now()}.sh`)
  writeFileSync(path, body)
  chmodSync(path, 0o755)
  return path
}

/**
 * "Re-sign in" must force a fresh login on an account that already reads as
 * signed in. Both halves used to be broken: the dialog opened straight onto
 * "✓ Signed in" with no way to start one, and the server left the old
 * `auth.json` in place, so a stuck/expired ChatGPT session could never be
 * replaced from the UI.
 */
test('re-sign in forces a fresh device login on an authenticated account', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const cli = writeFakeCli('tworuns', FAKE_CODEX_TWO_RUNS)
  await useFakeCodexCli(request, token, cli)

  await loadApp(page, token)
  const settings = await openSettings(page)
  const section = settings.getByTestId('codex-accounts-section')
  await section.getByTestId('codex-acct-add').click()
  const modal = page.getByTestId('codex-account-modal')
  await expect(modal).toBeVisible()
  await modal.getByTestId('codex-acct-name').fill('E2E Codex Resign')
  await modal.getByTestId('codex-acct-save').click()
  await expect(modal).toBeHidden()

  // First login: the fake CLI writes auth.json, so the account ends up
  // authenticated and the row offers "Re-sign in".
  const signIn = page.getByTestId('codex-signin-modal')
  await expect(signIn).toBeVisible()
  await signIn.getByTestId('codex-signin-start').click()
  await expect(signIn.getByTestId('codex-signin-done')).toBeVisible({ timeout: 20_000 })
  await signIn.getByTestId('codex-signin-close').click()

  const row = section
    .locator('[data-testid^="codex-acct-row-"]')
    .filter({ hasText: 'E2E Codex Resign' })
  const signInButton = row.locator('[data-testid^="codex-acct-signin-"]')
  await expect(signInButton).toHaveText('Re-sign in')

  // Second login: the same CLI now prints a different code and writes no
  // credentials — so "still signed in" can only mean the stale auth.json
  // survived.
  await signInButton.click()
  await expect(signIn).toBeVisible()
  await expect(signIn.getByTestId('codex-signin-resign-hint')).toBeVisible()
  await expect(signIn.getByTestId('codex-signin-done')).toHaveCount(0)

  await signIn.getByTestId('codex-signin-start').click()
  await expect(signIn.getByTestId('codex-signin-code')).toContainText('9ZZZ-QQQQQ', {
    timeout: 20_000,
  })
  // Starting the re-login stashed the old credentials, so the row is honest
  // about the account being signed out until the new login lands.
  await expect(row.locator('[data-testid^="codex-acct-unauth-"]')).toBeVisible({ timeout: 15_000 })

  await signIn.getByTestId('codex-signin-close').click()
  await row.locator('[data-testid^="codex-acct-delete-"]').click()
  const confirm = page.locator('.confirm-dialog')
  await expect(confirm).toBeVisible()
  await confirm.getByRole('button', { name: 'Delete' }).click()
  await expect(row).toHaveCount(0)

  // Specs share this server: put model discovery back the way we found it.
  await request.put('/api/plugins/codex/settings', {
    headers: { Authorization: `Bearer ${token}` },
    data: { updates: { discover_models: true } },
  })
})
