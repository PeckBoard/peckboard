import { expect, type APIRequestContext, type Page } from '../harness'
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
 * Shared plumbing for specs that drive real SSH terminals: a throwaway
 * OpenSSH daemon accepting the current user by key, a private Peckboard
 * (own data dir + port, ssh-fleet preinstalled) the spec may restart at
 * will, and helpers that read xterm's DOM-rendered screen.
 */

const testsDir = path.dirname(fileURLToPath(import.meta.url))
const repoRoot = path.resolve(testsDir, '..', '..', '..')
export const SERVER_BIN = process.env.PECKBOARD_E2E_BIN
  ? path.resolve(testsDir, '..', process.env.PECKBOARD_E2E_BIN)
  : path.join(repoRoot, 'target', 'verify', 'peckboard')
export const FLEET_WASM = path.join(repoRoot, 'peck-plugins', 'ssh-fleet', 'dist', 'plugin.wasm')

export async function freePort(): Promise<number> {
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

export function which(cmd: string): boolean {
  try {
    execFileSync('sh', ['-c', `command -v ${cmd}`], { stdio: 'ignore' })
    return true
  } catch {
    return false
  }
}

export type Sshd = { port: number; user: string; privateKey: string; child: ChildProcess }

export async function spawnSshd(): Promise<Sshd | null> {
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
export class Server {
  child: ChildProcess | null = null
  readonly dataDir = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-term-'))
  constructor(
    readonly port: number,
    readonly user: string,
    readonly pass: string,
  ) {
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
          PECKBOARD_BOOTSTRAP_USERNAME: this.user,
          PECKBOARD_BOOTSTRAP_PASSWORD: this.pass,
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

export async function login(
  request: APIRequestContext,
  base: string,
  username: string,
  password: string,
): Promise<string> {
  for (let i = 0; i < 40; i++) {
    const res = await request.post(`${base}/api/auth/login`, { data: { username, password } })
    if (res.ok()) return ((await res.json()) as { token: string }).token
    await new Promise((r) => setTimeout(r, 250))
  }
  throw new Error('login failed')
}

/** Approve ssh-fleet, finish setup, and register the local sshd as a host.
 *  Returns the host's label. */
export async function addFleetHost(
  request: APIRequestContext,
  base: string,
  token: string,
  sshd: Sshd,
): Promise<string> {
  const auth = { Authorization: `Bearer ${token}` }
  expect(
    (
      await request.post(`${base}/api/plugins/ssh-fleet/approval`, {
        headers: auth,
        data: { decision: 'approve' },
      })
    ).ok(),
  ).toBeTruthy()
  await request.post(`${base}/api/settings/setup/complete`, { headers: auth })
  const label = `term-host-${Date.now()}`
  const added = await request.post(`${base}/api/plugin-ui/ssh-fleet/hosts`, {
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
  return label
}

/** The visible terminal's screen as plain text (DOM renderer rows). */
export async function screen(scope: Page, terminalId?: string): Promise<string> {
  const sel = terminalId
    ? `[data-terminal-id="${terminalId}"] .xterm-rows`
    : '[data-testid="terminal-pane"] .xterm-rows'
  const texts = await scope.locator(sel).allInnerTexts()
  // xterm's DOM renderer draws spaces as non-breaking spaces.
  return texts.join('\n').replaceAll(String.fromCharCode(160), ' ')
}

export async function expectScreen(scope: Page, re: RegExp, terminalId?: string, timeout = 15_000) {
  await expect.poll(() => screen(scope, terminalId), { timeout }).toMatch(re)
}
