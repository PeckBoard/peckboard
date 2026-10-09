import { test, expect, type APIRequestContext, type Page } from '../harness'
import { spawn, execFileSync, type ChildProcess } from 'node:child_process'
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  writeFileSync,
} from 'node:fs'
import { createServer, connect } from 'node:net'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

/**
 * Interactive SSH terminals, end to end: core's terminal manager + JWT'd
 * `/ws/terminal/{id}`, the ssh-fleet plugin answering `terminal.host.*`,
 * xterm.js in a full-size app tab, tmux persistence across a real server
 * restart, and the pop-out window.
 *
 * The spec boots its OWN Peckboard (own data dir + port) so it can kill and
 * restart it without disturbing the shared suite server, and a throwaway
 * OpenSSH daemon accepting the current user by key. Skips cleanly when the
 * ssh-fleet wasm isn't built, OpenSSH isn't installed, or tmux isn't.
 */

const USER = 'term-user'
const PASS = 'term-password-1234'
const testsDir = path.dirname(fileURLToPath(import.meta.url))
const repoRoot = path.resolve(testsDir, '..', '..', '..')
const SERVER_BIN = process.env.PECKBOARD_E2E_BIN
  ? path.resolve(testsDir, '..', process.env.PECKBOARD_E2E_BIN)
  : path.join(repoRoot, 'target', 'verify', 'peckboard')
const FLEET_WASM = path.join(repoRoot, 'peck-plugins', 'ssh-fleet', 'dist', 'plugin.wasm')

async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = createServer()
    srv.on('error', reject)
    srv.listen(0, '127.0.0.1', () => {
      const addr = srv.address()
      const port = typeof addr === 'object' && addr ? addr.port : 0
      srv.close(() => resolve(port))
    })
  })
}

async function waitForPort(port: number, timeoutMs: number): Promise<boolean> {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    const ok = await new Promise<boolean>((resolve) => {
      const sock = connect({ port, host: '127.0.0.1' })
      sock.once('connect', () => {
        sock.destroy()
        resolve(true)
      })
      sock.once('error', () => resolve(false))
    })
    if (ok) return true
    await new Promise((r) => setTimeout(r, 100))
  }
  return false
}

function which(cmd: string): boolean {
  try {
    execFileSync('sh', ['-c', `command -v ${cmd}`], { stdio: 'ignore' })
    return true
  } catch {
    return false
  }
}

type Sshd = { port: number; user: string; privateKey: string; child: ChildProcess }

async function spawnSshd(): Promise<Sshd | null> {
  const sshd = ['/usr/sbin/sshd', '/usr/bin/sshd', '/sbin/sshd'].find((p) => existsSync(p))
  const keygen = ['/usr/bin/ssh-keygen', '/bin/ssh-keygen'].find((p) => existsSync(p))
  const user = process.env.USER ?? process.env.LOGNAME ?? ''
  if (!sshd || !keygen || !user) return null
  const dir = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-sshd-'))
  const hostKey = path.join(dir, 'hostkey')
  const clientKey = path.join(dir, 'id')
  for (const key of [hostKey, clientKey]) {
    execFileSync(keygen, ['-t', 'ed25519', '-N', '', '-q', '-f', key])
  }
  writeFileSync(path.join(dir, 'authorized_keys'), readFileSync(`${clientKey}.pub`))
  const port = await freePort()
  const config = path.join(dir, 'sshd_config')
  writeFileSync(
    config,
    [
      `Port ${port}`,
      'ListenAddress 127.0.0.1',
      `HostKey ${hostKey}`,
      `AuthorizedKeysFile ${path.join(dir, 'authorized_keys')}`,
      'StrictModes no',
      'UsePAM no',
      'PasswordAuthentication no',
      'KbdInteractiveAuthentication no',
      'PubkeyAuthentication yes',
      'LogLevel ERROR',
      '',
    ].join('\n'),
  )
  const child = spawn(sshd, ['-D', '-f', config, '-E', path.join(dir, 'sshd.log')], {
    stdio: 'ignore',
  })
  if (!(await waitForPort(port, 5000))) {
    child.kill()
    return null
  }
  return { port, user, privateKey: readFileSync(clientKey, 'utf8'), child }
}

/** A private Peckboard the spec may restart at will. */
class Server {
  child: ChildProcess | null = null
  readonly dataDir = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-term-'))
  constructor(readonly port: number) {
    mkdirSync(path.join(this.dataDir, 'plugins'), { recursive: true })
    copyFileSync(FLEET_WASM, path.join(this.dataDir, 'plugins', 'ssh-fleet.wasm'))
  }
  get base() {
    return `http://127.0.0.1:${this.port}`
  }
  async start() {
    this.child = spawn(
      SERVER_BIN,
      ['--port', String(this.port), '--https-port', String(this.port + 1), '--host', '127.0.0.1'],
      {
        env: {
          ...process.env,
          PECKBOARD_DATA_DIR: this.dataDir,
          PECKBOARD_BOOTSTRAP_USERNAME: USER,
          PECKBOARD_BOOTSTRAP_PASSWORD: PASS,
          PECKBOARD_NO_RESUME: '1',
          PECKBOARD_TTS_DOWNLOAD: '0',
        },
        stdio: 'ignore',
      },
    )
    const deadline = Date.now() + 30_000
    while (Date.now() < deadline) {
      try {
        if ((await fetch(`${this.base}/api/health`)).ok) return
      } catch {
        /* not up yet */
      }
      await new Promise((r) => setTimeout(r, 200))
    }
    throw new Error('private server never came up')
  }
  async stop() {
    const child = this.child
    if (!child || child.exitCode !== null) return
    const exited = new Promise((r) => child.once('exit', r))
    child.kill('SIGTERM')
    await Promise.race([exited, new Promise((r) => setTimeout(r, 10_000))])
    if (child.exitCode === null) child.kill('SIGKILL')
    this.child = null
  }
}

async function login(request: APIRequestContext, base: string): Promise<string> {
  for (let i = 0; i < 40; i++) {
    const res = await request.post(`${base}/api/auth/login`, {
      data: { username: USER, password: PASS },
    })
    if (res.ok()) return ((await res.json()) as { token: string }).token
    await new Promise((r) => setTimeout(r, 250))
  }
  throw new Error('login failed')
}

/** The visible terminal's screen as plain text (DOM renderer rows). */
async function screen(scope: Page, terminalId?: string): Promise<string> {
  const sel = terminalId
    ? `[data-terminal-id="${terminalId}"] .xterm-rows`
    : '[data-testid="terminal-pane"] .xterm-rows'
  const texts = await scope.locator(sel).allInnerTexts()
  // xterm's DOM renderer draws spaces as non-breaking spaces.
  return texts.join('\n').replaceAll(String.fromCharCode(160), ' ')
}

async function expectScreen(scope: Page, re: RegExp, terminalId?: string, timeout = 15_000) {
  await expect.poll(() => screen(scope, terminalId), { timeout }).toMatch(re)
}

test('a real shell in an app tab: keystrokes, completion, vim, ^C, reload, server restart, pop-out', async ({
  request,
  page,
}) => {
  test.setTimeout(240_000)
  test.skip(!existsSync(FLEET_WASM), 'ssh-fleet plugin wasm not built')
  test.skip(!existsSync(SERVER_BIN), 'peckboard binary not built')
  test.skip(!which('tmux'), 'tmux not installed')
  const sshd = await spawnSshd()
  test.skip(!sshd, 'OpenSSH sshd/ssh-keygen not available')
  if (!sshd) return

  const server = new Server(await freePort())
  let terminalId = ''
  let token = ''
  try {
    await server.start()
    token = await login(request, server.base)
    const auth = { Authorization: `Bearer ${token}` }
    expect(
      (
        await request.post(`${server.base}/api/plugins/ssh-fleet/approval`, {
          headers: auth,
          data: { decision: 'approve' },
        })
      ).ok(),
    ).toBeTruthy()
    await request.post(`${server.base}/api/settings/setup/complete`, { headers: auth })
    const label = `term-host-${Date.now()}`
    const added = await request.post(`${server.base}/api/plugin-ui/ssh-fleet/hosts`, {
      headers: auth,
      data: {
        label,
        hostname: '127.0.0.1',
        port: sshd.port,
        username: sshd.user,
        private_key: sshd.privateKey,
      },
    })
    expect(added.ok(), `add host failed: ${await added.text()}`).toBeTruthy()

    // ── New terminal → searchable host picker → a full-size tab ─────────
    await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
    await page.goto(`${server.base}/`)
    await page.getByTestId('rail-terminals').click()
    await expect(page.getByTestId('terminals-page')).toBeVisible()
    await page.getByTestId('terminal-new').click()
    const search = page.getByTestId('terminal-host-search')
    await expect(search).toBeVisible({ timeout: 10_000 })
    await search.fill(label.slice(0, 12))
    await search.press('Enter')
    const view = page.locator('[data-testid="terminal-view"]:visible')
    await expect(view).toBeVisible({ timeout: 10_000 })
    terminalId = (await view.getAttribute('data-terminal-id')) ?? ''
    expect(terminalId).not.toBe('')
    await expect(page.locator(`[data-tab-key="terminal:${terminalId}"]`)).toBeVisible()
    await expect(view.getByTestId('terminal-pane')).toHaveAttribute('data-phase', 'live', {
      timeout: 20_000,
    })
    await expect(page.getByTestId('terminal-not-persistent')).toHaveCount(0)
    // The pane fills the content area — not a cramped box.
    const box = await view.boundingBox()
    const vp = page.viewportSize()
    expect(box && vp && box.height > vp.height * 0.7).toBeTruthy()

    // ── Keystrokes stream one by one: echo shows before Enter ───────────
    await page.keyboard.type('echo hel', { delay: 40 })
    await expectScreen(page, /echo hel/, terminalId)
    await page.keyboard.type('lo-$((6*7))', { delay: 20 })
    await page.keyboard.press('Enter')
    await expectScreen(page, /hello-42/, terminalId)

    // ── Tab completion ──────────────────────────────────────────────────
    const stem = `/tmp/peckcomp-${Date.now()}`
    await page.keyboard.type(`touch ${stem}-zebra\n`)
    await page.keyboard.type(`ls ${stem}-ze`)
    await page.keyboard.press('Tab')
    await expectScreen(page, new RegExp(`ls ${stem}-zebra`), terminalId)
    await page.keyboard.press('Enter')

    // ── vim opens full-screen and quits back to the prompt ──────────────
    if (which('vim')) {
      await page.keyboard.type('vim /tmp/peck-vim-e2e.txt\n')
      await expectScreen(page, /peck-vim-e2e\.txt/, terminalId)
      await expectScreen(page, /^~/m, terminalId)
      await page.keyboard.press('Escape')
      await page.keyboard.type(':q!\n')
    }
    await page.keyboard.type('echo after-vim-$((1+1))\n')
    await expectScreen(page, /after-vim-2/, terminalId)

    // ── Ctrl+C interrupts a running command ─────────────────────────────
    await page.keyboard.type('sleep 100\n')
    await page.waitForTimeout(500)
    await page.keyboard.press('Control+c')
    await page.keyboard.type('echo intr-$((2+3))\n')
    await expectScreen(page, /intr-5/, terminalId, 5_000)

    // State that only a persistent shell keeps.
    await page.keyboard.type('export PECK_E2E=persisted\n')

    // ── Reload: the tab comes back and replays what was on screen ───────
    await page.reload()
    await expectScreen(page, /hello-42/, terminalId)

    // ── Restart the server: the same tmux shell reattaches ──────────────
    const pane = page.locator(`[data-terminal-id="${terminalId}"] [data-testid="terminal-pane"]`)
    await server.stop()
    // The tab notices the server is gone…
    await expect(pane).not.toHaveAttribute('data-phase', 'live', { timeout: 15_000 })
    await server.start()
    // …and reconnects on its own once it's back, reattaching the shell.
    await expect(pane).toHaveAttribute('data-phase', 'live', { timeout: 45_000 })
    await page.locator(`[data-terminal-id="${terminalId}"] .xterm`).click()
    await page.keyboard.type('echo "v-$PECK_E2E"\n')
    await expectScreen(page, /v-persisted/, terminalId, 20_000)

    // ── Pop out: its own window, the same live shell ────────────────────
    const popupPromise = page.waitForEvent('popup')
    await page.locator(`[data-terminal-id="${terminalId}"]`).getByTestId('terminal-popout').click()
    const popup = await popupPromise
    await expect(popup.getByTestId('terminal-popout-window')).toBeVisible()
    await expect(popup.getByTestId('terminal-pane')).toHaveAttribute('data-phase', 'live', {
      timeout: 20_000,
    })
    await expectScreen(popup, /v-persisted/)
    await popup.locator('.xterm').click()
    await popup.keyboard.type('echo pop-$((3*3))\n')
    await expectScreen(popup, /pop-9/)
    await expectScreen(page, /pop-9/, terminalId)
    await popup.close()
  } finally {
    if (terminalId && token && server.child) {
      await request
        .delete(`${server.base}/api/terminals/${terminalId}`, {
          headers: { Authorization: `Bearer ${token}` },
        })
        .catch(() => {})
      await new Promise((r) => setTimeout(r, 1000))
    }
    await server.stop()
    sshd.child.kill()
  }
})
