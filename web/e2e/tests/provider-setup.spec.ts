import { test, expect, type Page } from '../harness'
import { spawn, type ChildProcess } from 'node:child_process'
import { chmodSync, existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { SERVER_BIN, freePort, login } from './ssh-harness'

/**
 * First-run provider onboarding, on a genuinely fresh install: no provider is
 * on, no CLI exists, and the setup wizard appears by itself.
 *
 * A private server runs with a fake `curl` first on PATH and an empty HOME, so
 * the real install, activation, sign-in and status code all run. The fake curl
 * serves an install script that copies a fake CLI to where that vendor's real
 * installer puts it, so this also proves the server finds Grok in `~/.grok/bin`
 * on a PATH that doesn't include it. The fake CLIs print what the real ones
 * print and "sign in" a couple of seconds later.
 */

const ADMIN = 'setup-admin'
const FIRST_PASS = 'first-password-1234'
const NEW_PASS = 'second-password-5678'

const BASH = '#!/bin/bash\n'

/** Prints to stderr or stdout, then writes the credential a real CLI would. */
function deviceCli(version: string, loginArgs: string, prompt: string, credential: string) {
  return `${BASH}case "$*" in
  --version) echo "${version}" ;;
  "${loginArgs}")
    ${prompt}
    sleep 2
    mkdir -p "$(dirname "${credential}")"
    echo '{"token":"fake"}' > "${credential}"
    ;;
  *) exit 1 ;;
esac
`
}

const FAKE_CLIS: Record<string, string> = {
  claude: `${BASH}case "$*" in
  --version) echo "9.9.9 (Claude Code)" ;;
  "auth status --json") echo '{"loggedIn":false}' ;;
  *) exit 1 ;;
esac
`,
  codex: deviceCli(
    'codex-cli 9.9.9',
    'login --device-auth',
    'echo "Open https://auth.openai.com/codex/device and enter ABCD-EFGH"',
    '$CODEX_HOME/auth.json',
  ),
  grok: deviceCli(
    'grok 9.9.9',
    'login --device-auth',
    'echo "Visit https://accounts.x.ai/oauth2/device?user_code=FAKE" >&2',
    '$GROK_HOME/auth.json',
  ),
  kimi: deviceCli(
    '9.9.9',
    'login',
    'echo "Opening browser for Kimi device login: https://www.kimi.com/code/authorize_device?user_code=FAKE" >&2',
    '$KIMI_CODE_HOME/config.toml',
  ),
  'cursor-agent': `${BASH}auth="$HOME/.cursor/fake-auth"
case "$*" in
  --version) echo "2026.01.01-fake" ;;
  "status --format json")
    if [ -f "$auth" ]; then echo '{"isAuthenticated":true}'
    else echo '{"isAuthenticated":false,"message":"Not logged in"}'; fi
    ;;
  login)
    echo "Starting login process..."
    echo "Open a browser and navigate to this link: https://cursor.com/loginDeepControl?challenge=fake"
    sleep 2
    mkdir -p "$(dirname "$auth")"
    touch "$auth"
    ;;
  *) exit 1 ;;
esac
`,
}

/** The fake `curl`: picks the vendor from the URL. Kimi's download fails until
 *  the spec removes the marker file, to exercise failure and retry. */
function fakeCurl(fakesDir: string): string {
  return `${BASH}for a in "$@"; do case "$a" in https://*) url="$a" ;; esac; done
case "$url" in
  *claude.ai*) name=claude; dir=.local/bin ;;
  *chatgpt.com*) name=codex; dir=.local/bin ;;
  *cursor.com*) name=cursor-agent; dir=.local/bin ;;
  *x.ai*) name=grok; dir=.grok/bin ;;
  *kimi.com*)
    if [ -f "${fakesDir}/kimi-download-fails" ]; then
      echo "curl: (22) The requested URL returned error: 500" >&2
      exit 22
    fi
    name=kimi; dir=.kimi-code/bin
    ;;
  *) echo "unexpected url: $url" >&2; exit 2 ;;
esac
cat <<EOF
echo "Downloading $name"
mkdir -p "\\$HOME/$dir"
cp "${fakesDir}/$name" "\\$HOME/$dir/$name"
chmod +x "\\$HOME/$dir/$name"
echo "$name installed"
EOF
`
}

let root: string
let fakesDir: string
let base: string
let child: ChildProcess

test.describe.configure({ mode: 'serial' })

test.beforeAll(async () => {
  root = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-onboard-'))
  const dataDir = path.join(root, 'data')
  const home = path.join(root, 'home')
  fakesDir = path.join(root, 'fakes')
  const bin = path.join(root, 'bin')
  for (const d of [dataDir, home, fakesDir, bin]) mkdirSync(d)

  for (const [name, script] of Object.entries(FAKE_CLIS)) {
    writeFileSync(path.join(fakesDir, name), script)
    chmodSync(path.join(fakesDir, name), 0o755)
  }
  writeFileSync(path.join(fakesDir, 'kimi-download-fails'), '')
  writeFileSync(path.join(bin, 'curl'), fakeCurl(fakesDir))
  chmodSync(path.join(bin, 'curl'), 0o755)

  const port = await freePort()
  base = `http://127.0.0.1:${port}`
  const env: NodeJS.ProcessEnv = {
    ...process.env,
    HOME: home,
    PATH: `${bin}:/usr/bin:/bin`,
    PECKBOARD_DATA_DIR: dataDir,
    PECKBOARD_BOOTSTRAP_USERNAME: ADMIN,
    PECKBOARD_BOOTSTRAP_PASSWORD: FIRST_PASS,
    PECKBOARD_NO_RESUME: '1',
    PECKBOARD_TTS_DOWNLOAD: '0',
  }
  // The shared harness pre-activates every plugin. This is a fresh install.
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

async function openWizardProviders(page: Page, token: string) {
  await page.addInitScript((t) => {
    if (!localStorage.getItem('peckboard_token')) localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(base)
  await expect(page.getByTestId('setup-wizard')).toBeVisible({ timeout: 10_000 })
  await page.getByTestId('setup-pw-current').fill(FIRST_PASS)
  await page.getByTestId('setup-pw-new').fill(NEW_PASS)
  await page.getByTestId('setup-pw-confirm').fill(NEW_PASS)
  await page.getByTestId('setup-next').click()
  await expect(page.getByTestId('provider-setup')).toBeVisible({ timeout: 15_000 })
}

/** Install one provider and wait until it is asking for sign-in. */
async function install(page: Page, id: string, version: string) {
  await expect(page.getByTestId(`provider-setup-status-${id}`)).toHaveText('Not installed')
  await page.getByTestId(`provider-setup-install-${id}`).click()
  await expect(page.getByTestId(`provider-setup-signin-${id}`)).toBeVisible({ timeout: 30_000 })
  await expect(page.getByTestId(`provider-setup-status-${id}`)).toContainText(version)
}

test('fresh install: install and sign in every CLI from the wizard, no agent or terminal', async ({
  page,
  request,
}) => {
  test.setTimeout(180_000)
  const token = await login(request, base, ADMIN, FIRST_PASS)
  await openWizardProviders(page, token)

  // Nothing connected yet: the step says so, but doesn't trap anyone.
  await expect(page.getByTestId('setup-no-provider-hint')).toBeVisible()
  await expect(page.getByTestId('setup-next')).toBeEnabled()
  await expect(page.getByTestId('provider-setup-plan-claude')).toHaveText('Uses Claude Pro or Max')
  await expect(page.getByTestId('provider-setup-plan-grok')).toHaveText(
    'Uses SuperGrok or X Premium+',
  )
  // The install command waits behind Details (Install runs it for the user).
  await expect(page.getByTestId('provider-setup-command-grok')).toHaveCount(0)
  await expect(page.getByTestId('provider-setup')).not.toContainText('https://x.ai/cli/install.sh')
  await page.getByTestId('provider-setup-details-grok').click()
  await expect(page.getByTestId('provider-setup-details-grok')).toHaveAttribute(
    'aria-expanded',
    'true',
  )
  await expect(page.getByTestId('provider-setup-command-grok')).toContainText(
    'https://x.ai/cli/install.sh',
  )
  await page.getByTestId('provider-setup-details-grok').click()
  await expect(page.getByTestId('provider-setup-command-grok')).toHaveCount(0)
  // No default AI line until there are models to pick from.
  await expect(page.getByTestId('setup-default-ai')).toHaveCount(0)

  // ── Cursor: the new host login ───────────────────────────────────
  await install(page, 'cursor', '2026.01.01-fake')
  await page.getByTestId('provider-setup-signin-cursor').click()
  await page.getByTestId('cursor-signin-start').click()
  await expect(page.getByTestId('cursor-signin-url')).toHaveAttribute(
    'href',
    'https://cursor.com/loginDeepControl?challenge=fake',
  )
  await expect(page.getByTestId('cursor-signin-done')).toBeVisible({ timeout: 20_000 })
  await page.getByTestId('cursor-signin-close').click()
  await expect(page.getByTestId('provider-setup-status-cursor')).toContainText('✓ Ready')
  // The first connected provider clears the "nothing connected" hint.
  await expect(page.getByTestId('setup-no-provider-hint')).toHaveCount(0)

  // The first ready provider fills in the default AI by itself.
  await expect(page.getByTestId('setup-default-ai-name')).not.toHaveText('none yet', {
    timeout: 15_000,
  })
  const pickToken = await login(request, base, ADMIN, NEW_PASS)
  let autoPicked = ''
  await expect
    .poll(async () => {
      const res = await request.get(`${base}/api/settings/default-model`, {
        headers: { Authorization: `Bearer ${pickToken}` },
      })
      autoPicked = ((await res.json()) as { model?: string }).model ?? ''
      return autoPicked
    })
    .toMatch(/^cursor:/)

  // ── Codex: device code, on an account created for the user ───────
  await install(page, 'codex', 'codex-cli 9.9.9')
  await page.getByTestId('provider-setup-signin-codex').click()
  await page.getByTestId('codex-signin-start').click()
  await expect(page.getByTestId('codex-signin-code')).toContainText('ABCD-EFGH')
  await expect(page.getByTestId('codex-signin-done')).toBeVisible({ timeout: 20_000 })
  await page.getByTestId('codex-signin-close').click()
  await expect(page.getByTestId('provider-setup-status-codex')).toContainText('✓ Ready as ChatGPT')

  // ── Grok: installs to ~/.grok/bin, which isn't on the server's PATH ──
  await install(page, 'grok', 'grok 9.9.9')
  await page.getByTestId('provider-setup-signin-grok').click()
  await page.getByTestId('grok-signin-start').click()
  await expect(page.getByTestId('grok-signin-url')).toBeVisible({ timeout: 20_000 })
  await expect(page.getByTestId('grok-signin-done')).toBeVisible({ timeout: 20_000 })
  await page.getByTestId('grok-signin-close').click()
  await expect(page.getByTestId('provider-setup-status-grok')).toContainText('✓ Ready as Grok')

  // ── Kimi: a failed download is a visible failure, and retry works ──
  await page.getByTestId('provider-setup-install-kimi').click()
  await expect(page.getByTestId('provider-setup-error-kimi')).toContainText('exit code 22', {
    timeout: 30_000,
  })
  await expect(page.getByTestId('provider-setup-log-kimi')).toContainText('returned error: 500')
  await expect(page.getByTestId('provider-setup-status-kimi')).toHaveText('Not installed')
  rmSync(path.join(fakesDir, 'kimi-download-fails'))
  await page.getByTestId('provider-setup-install-kimi').click()
  await expect(page.getByTestId('provider-setup-signin-kimi')).toBeVisible({ timeout: 30_000 })
  await page.getByTestId('provider-setup-signin-kimi').click()
  await page.getByTestId('kimi-signin-start').click()
  await expect(page.getByTestId('kimi-signin-done')).toBeVisible({ timeout: 20_000 })
  await page.getByTestId('kimi-signin-close').click()
  await expect(page.getByTestId('provider-setup-status-kimi')).toContainText('✓ Ready as Kimi')

  // ── Claude: the short dialog, without the budget form ────────────
  await install(page, 'claude', '9.9.9 (Claude Code)')
  await page.getByTestId('provider-setup-signin-claude').click()
  const dialog = page.getByTestId('claude-account-modal')
  await expect(dialog).toContainText('Sign in to Claude')
  await expect(page.getByTestId('acct-name')).toHaveCount(0)
  await expect(page.getByTestId('acct-window')).toHaveCount(0)
  await expect(page.getByTestId('acct-login-start')).toHaveText('Sign in with Claude')
  // The browser login needs Anthropic, so finish through the API-key path,
  // which lands on the same account row.
  await page.getByTestId('acct-kind-api_key').click()
  await page.getByTestId('acct-credential').fill('sk-ant-fake-key')
  await page.getByTestId('acct-save').click()
  await expect(dialog).toBeHidden()
  await expect(page.getByTestId('provider-setup-status-claude')).toContainText('✓ Ready as Claude')

  // ── Self-hosted models just turn on ─────────────────────────────────
  await expect(page.getByTestId('provider-setup-more')).toHaveText('Self-hosted models (advanced)')
  await page.getByTestId('provider-setup-more').click()
  await page.getByTestId('provider-setup-enable-ollama').click()
  await expect(page.getByTestId('provider-setup')).toContainText('✓ On')

  // ── The auto-picked default AI stuck: later sign-ins didn't move it ──
  const freshToken = await login(request, base, ADMIN, NEW_PASS)
  const auth = { Authorization: `Bearer ${freshToken}` }
  expect(
    (
      (await (
        await request.get(`${base}/api/settings/default-model`, { headers: auth })
      ).json()) as { model?: string }
    ).model,
  ).toBe(autoPicked)

  // ── Where files live: the ready-made folder in the server user's home ──
  await page.getByTestId('setup-next').click()
  const suggested = path.join(root, 'home', 'PeckBoard')
  await expect(page.getByTestId('setup-folder-default-path')).toHaveText(suggested)
  await expect(page.getByTestId('setup-no-folder-hint')).toBeVisible()
  await page.getByTestId('setup-folder-use-default').click()
  await expect(page.getByTestId('setup-folder-done')).toContainText('My work')
  await expect(page.getByTestId('setup-folder-done')).toContainText(suggested)
  expect(existsSync(suggested), 'Use this folder must create the directory').toBe(true)
  const folders = (await (await request.get(`${base}/api/folders`, { headers: auth })).json()) as {
    name: string
    path: string
  }[]
  expect(folders.map((f) => f.path)).toContain(suggested)

  // ── Finish, then the same panel is in Settings ───────────────────
  await page.getByTestId('setup-next').click()
  await page.getByTestId('setup-finish').click()
  await expect(page.getByTestId('setup-wizard')).toBeHidden({ timeout: 10_000 })

  await page.goto(`${base}/settings`)
  await page.getByTestId('settings-nav-providers').click()
  await expect(page.getByTestId('provider-setup-status-cursor')).toContainText('✓ Ready', {
    timeout: 15_000,
  })
  await expect(page.getByTestId('provider-setup-plan-cursor')).toHaveText('Uses a Cursor plan')
  // Settings carries the hide switch on each turned-on provider, not the
  // wizard's default-AI line.
  await expect(page.getByTestId('provider-toggle-cursor')).not.toBeChecked()
  await expect(page.getByTestId('setup-default-ai')).toHaveCount(0)
  await expect(page.getByTestId('claude-settings-section')).toBeVisible()
})
test('provider setup is admin-only and only ever runs its own table', async ({ request }) => {
  const adminToken = await login(request, base, ADMIN, NEW_PASS)
  const admin = { Authorization: `Bearer ${adminToken}` }

  const unknown = await request.post(`${base}/api/provider-setup/not-a-provider/install`, {
    headers: admin,
  })
  expect(unknown.status()).toBe(404)

  const password = 'plain-user-password-1234'
  const create = await request.post(`${base}/api/users`, {
    headers: admin,
    data: { username: 'plain-user', password, role: 'user' },
  })
  expect(create.ok(), await create.text()).toBeTruthy()
  const userToken = await login(request, base, 'plain-user', password)
  const user = { Authorization: `Bearer ${userToken}` }

  expect((await request.get(`${base}/api/provider-setup`, { headers: user })).status()).toBe(403)
  expect(
    (await request.post(`${base}/api/provider-setup/claude/install`, { headers: user })).status(),
  ).toBe(403)
})
