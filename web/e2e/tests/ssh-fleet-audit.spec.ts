import { test, expect, type APIRequestContext } from '../harness'
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
 * SSH Fleet audit trail, end to end: a `mock:mcp` session runs a REAL
 * `ssh_run` (with a `reason`) against a throwaway local sshd; the SSH Fleet
 * page then lists that call with its reason and a link to the session, the
 * link opens the session in the app, and the page offers no way to run a
 * command itself.
 *
 * Boots its own Peckboard with the ssh-fleet wasm staged (like
 * ssh-terminal.spec.ts). Skips cleanly when the wasm isn't built or OpenSSH
 * isn't installed.
 */

const USER = 'fleet-user'
const PASS = 'fleet-password-1234'
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

type Sshd = { port: number; user: string; privateKey: string; child: ChildProcess }

async function spawnSshd(): Promise<Sshd | null> {
  const sshd = ['/usr/sbin/sshd', '/usr/bin/sshd', '/sbin/sshd'].find((p) => existsSync(p))
  const keygen = ['/usr/bin/ssh-keygen', '/bin/ssh-keygen'].find((p) => existsSync(p))
  const user = process.env.USER ?? process.env.LOGNAME ?? ''
  if (!sshd || !keygen || !user) return null
  const dir = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-fleet-sshd-'))
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

async function startServer(port: number): Promise<{ child: ChildProcess; base: string }> {
  const dataDir = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-fleet-'))
  mkdirSync(path.join(dataDir, 'plugins'), { recursive: true })
  copyFileSync(FLEET_WASM, path.join(dataDir, 'plugins', 'ssh-fleet.wasm'))
  const child = spawn(
    SERVER_BIN,
    ['--port', String(port), '--https-port', String(port + 1), '--host', '127.0.0.1'],
    {
      env: {
        ...process.env,
        PECKBOARD_DATA_DIR: dataDir,
        PECKBOARD_BOOTSTRAP_USERNAME: USER,
        PECKBOARD_BOOTSTRAP_PASSWORD: PASS,
        PECKBOARD_NO_RESUME: '1',
        // Seeds the bundled crate plugins (incl. the mock provider) installed.
        PECKBOARD_PREINSTALL_PLUGINS: 'all',
        PECKBOARD_TTS_DOWNLOAD: '0',
      },
      stdio: 'ignore',
    },
  )
  const base = `http://127.0.0.1:${port}`
  const deadline = Date.now() + 30_000
  while (Date.now() < deadline) {
    try {
      if ((await fetch(`${base}/api/health`)).ok) return { child, base }
    } catch {
      /* not up yet */
    }
    await new Promise((r) => setTimeout(r, 200))
  }
  child.kill('SIGKILL')
  throw new Error('private server never came up')
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

function mcpBlock(tool: string, args: Record<string, unknown>): string {
  return '```mcp\n' + JSON.stringify({ tool, args }) + '\n```'
}

type Activity = { tool: string; reason?: string; session_id?: string; exit_code?: number }

test('an agent ssh_run shows on the SSH Fleet page with its reason and a session link', async ({
  request,
  page,
}) => {
  test.setTimeout(120_000)
  test.skip(!existsSync(FLEET_WASM), 'ssh-fleet plugin wasm not built')
  test.skip(!existsSync(SERVER_BIN), 'peckboard binary not built')
  const sshd = await spawnSshd()
  test.skip(!sshd, 'OpenSSH sshd/ssh-keygen not available')
  if (!sshd) return

  const server = await startServer(await freePort())
  try {
    const token = await login(request, server.base)
    const auth = { Authorization: `Bearer ${token}` }
    const approve = await request.post(`${server.base}/api/plugins/ssh-fleet/approval`, {
      headers: auth,
      data: { decision: 'approve' },
    })
    expect(approve.ok(), `approve failed: ${await approve.text()}`).toBeTruthy()
    await request.post(`${server.base}/api/settings/setup/complete`, { headers: auth })

    const label = `audit-host-${Date.now()}`
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

    const folderRes = await request.post(`${server.base}/api/folders`, {
      headers: auth,
      data: {
        name: `fleet-audit-${Date.now()}`,
        path: mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-fleet-folder-')),
      },
    })
    expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
    const folder = (await folderRes.json()) as { id: string }
    const sessionName = 'Fleet auditor'
    const sessionRes = await request.post(`${server.base}/api/sessions`, {
      headers: auth,
      data: { name: sessionName, folder_id: folder.id, model: 'mock:mcp' },
    })
    expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
    const session = (await sessionRes.json()) as { id: string }

    // ── The agent runs a real command over SSH, stating why ─────────────
    const reason = 'Confirm the audit marker before the rollout'
    const send = await request.post(`${server.base}/api/sessions/${session.id}/message`, {
      headers: auth,
      data: {
        text: `check it\n${mcpBlock('ssh_run', { host: label, command: 'echo audit-$((6*7))', reason })}`,
        model: 'mock:mcp',
      },
    })
    expect(send.ok(), `send failed: ${await send.text()}`).toBeTruthy()
    await expect
      .poll(
        async () => {
          const res = await request.get(
            `${server.base}/api/plugin-ui/ssh-fleet/activity?host=all&since=0`,
            { headers: auth },
          )
          const items = ((await res.json()) as { items: Activity[] }).items
          return items.find((a) => a.tool === 'ssh_run') ?? null
        },
        { timeout: 30_000 },
      )
      .toMatchObject({ reason, session_id: session.id, exit_code: 0 })

    // ── The page shows it: reason + session link; no command input ──────
    await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
    await page.goto(`${server.base}/plugin-page/ssh-fleet/ssh-fleet`)
    const frame = page.frameLocator('[data-testid="plugin-fullpage-frame"]')
    const row = frame.getByTestId('activity-row').filter({ hasText: 'echo audit-' })
    await expect(row).toBeVisible({ timeout: 15_000 })
    await expect(row.getByTestId('activity-reason')).toContainText(reason)
    await expect(row.getByTestId('activity-status')).toContainText('exit 0')
    await expect(frame.getByTestId('host-row').filter({ hasText: label })).toBeVisible()
    // The dashboard cannot run commands: no command box, no Run button.
    // Every visible text field is a search/filter box.
    const fields = frame.locator('textarea:visible, input:visible:not([type="checkbox"])')
    for (const ph of await fields.evaluateAll((els) =>
      els.map((e) => e.getAttribute('placeholder') ?? ''),
    )) {
      expect(ph).toMatch(/search|^all /i)
    }
    await expect(frame.getByPlaceholder(/command to run/i)).toHaveCount(0)
    await expect(frame.getByRole('button', { name: /^run$/i })).toHaveCount(0)
    // Output is one click away.
    await row.getByTestId('activity-output-toggle').click()
    await expect(row.locator('pre')).toContainText('audit-42')

    // ── The session link opens that session in the app ──────────────────
    const link = row.getByTestId('activity-session')
    await expect(link).toContainText(sessionName)
    await link.click()
    await expect(page).toHaveURL(new RegExp(`/sessions/${session.id}`), { timeout: 10_000 })
    await expect(page.locator('.chat-toolbar')).toBeVisible({ timeout: 10_000 })
  } finally {
    server.child.kill('SIGTERM')
    sshd.child.kill()
  }
})
