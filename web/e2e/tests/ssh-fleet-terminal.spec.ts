import { test, expect, type APIRequestContext, type Page } from '../harness'
import { spawn, execFileSync, type ChildProcess } from 'node:child_process'
import { existsSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs'
import { createServer } from 'node:net'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * SSH Fleet's interactive terminal, end to end through the real stack: the
 * ssh-fleet WASM plugin (its dashboard iframe + authed routes), core's
 * `peckboard_ssh_term_*` host functions and `/ws/terminal`, and xterm.js
 * served from `/vendor/xterm/*`.
 *
 * Needs a real `sshd`: the spec spawns a throwaway OpenSSH daemon on an
 * ephemeral loopback port (host key, client key, and config in a temp dir)
 * and registers it as a fleet host authenticating as the current user by
 * inline key — the same fixture the Rust `ssh.rs` tests use. Skips cleanly
 * when OpenSSH is not installed or the ssh-fleet plugin blob is not staged
 * (its source lives in its own repo: `peck-plugins/ssh-fleet/dist/plugin.wasm`
 * must have been built for playwright.config.ts to copy it in).
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

function firstExisting(candidates: string[]): string | null {
  return candidates.find((c) => existsSync(c)) ?? null
}

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
  const { connect } = await import('node:net')
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

type LocalSshd = { port: number; user: string; privateKey: string; child: ChildProcess }

/** Spawn a throwaway sshd accepting the current user by key; null = skip. */
async function spawnSshd(): Promise<LocalSshd | null> {
  const sshd = firstExisting(['/usr/sbin/sshd', '/usr/bin/sshd', '/sbin/sshd'])
  const keygen = firstExisting(['/usr/bin/ssh-keygen', '/bin/ssh-keygen'])
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

async function pluginInstalled(request: APIRequestContext, token: string): Promise<boolean> {
  const res = await request.get('/api/plugins', { headers: { Authorization: `Bearer ${token}` } })
  if (!res.ok()) return false
  const body = (await res.json()) as {
    wasm_plugins?: Array<{ name: string; status: string }>
    sidebar_items?: Array<{ plugin: string }>
  }
  return (
    (body.wasm_plugins ?? []).some((p) => p.name === 'ssh-fleet' && p.status === 'approved') &&
    (body.sidebar_items ?? []).some((i) => i.plugin === 'ssh-fleet')
  )
}

async function openFleetPage(page: Page, token: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/plugin-page/ssh-fleet/ssh-fleet')
  const frame = page.frameLocator('[data-testid="plugin-fullpage-frame"][data-plugin="ssh-fleet"]')
  await expect(frame.getByTestId('tab-activity')).toBeVisible({ timeout: 15_000 })
  return frame
}

test('open a terminal, type into the live shell, reattach after a reload, see it exit, close it', async ({
  request,
  page,
}) => {
  test.setTimeout(120_000)
  const token = await authenticate(request)
  test.skip(!(await pluginInstalled(request, token)), 'ssh-fleet plugin blob not staged')
  const sshd = await spawnSshd()
  test.skip(!sshd, 'OpenSSH sshd/ssh-keygen not available')
  if (!sshd) return

  try {
    // Register the throwaway daemon as a fleet host (inline key auth).
    const label = `e2e-term-${Date.now()}`
    const added = await request.post('/api/plugin-ui/ssh-fleet/hosts', {
      headers: { Authorization: `Bearer ${token}` },
      data: {
        label,
        hostname: '127.0.0.1',
        port: sshd.port,
        username: sshd.user,
        private_key: sshd.privateKey,
      },
    })
    expect(added.ok(), `add host failed: ${await added.text()}`).toBeTruthy()
    const { host } = (await added.json()) as { host: { id: string } }

    // ── Open: a real xterm renders and the shell answers what we type ────
    const frame = await openFleetPage(page, token)
    const row = frame.locator(`[data-testid="host-row"][data-host="${host.id}"]`)
    await expect(row).toBeVisible()
    await row.hover()
    await row.getByTestId('open-terminal').click()

    const tab = frame.getByTestId('term-tab')
    await expect(tab).toHaveCount(1)
    await expect(tab).toContainText(label)
    const pane = frame.getByTestId('term-pane')
    await expect(pane.locator('.xterm')).toBeVisible({ timeout: 20_000 })
    await expect(pane.getByTestId('term-status')).toHaveText('live', { timeout: 20_000 })

    // The marker is computed by the shell so the output line cannot be
    // mistaken for the echoed input line.
    const input = pane.locator('.xterm-helper-textarea')
    await input.focus()
    await input.pressSequentially("printf 'e2e-%s\\n' marker-ok", { delay: 5 })
    await input.press('Enter')
    const rows = pane.locator('.xterm-rows')
    await expect(rows).toContainText('e2e-marker-ok', { timeout: 20_000 })

    // Interactive input works too: ^C ends a sleeping foreground job and the
    // shell prompts again (a second marker proves it is still alive).
    await input.pressSequentially('sleep 30', { delay: 5 })
    await input.press('Enter')
    await input.press('Control+c')
    await input.pressSequentially("printf 'e2e-%s\\n' after-interrupt", { delay: 5 })
    await input.press('Enter')
    await expect(rows).toContainText('e2e-after-interrupt', { timeout: 20_000 })

    // ── Reattach: the shell survives a full page reload; its scrollback
    //    replays into the fresh view ────────────────────────────────────
    await page.reload()
    const frame2 = await openFleetPage(page, token)
    const tab2 = frame2.getByTestId('term-tab')
    await expect(tab2).toHaveCount(1, { timeout: 15_000 })
    await tab2.click()
    const pane2 = frame2.getByTestId('term-pane')
    await expect(pane2.locator('.xterm')).toBeVisible({ timeout: 20_000 })
    const rows2 = pane2.locator('.xterm-rows')
    await expect(rows2).toContainText('e2e-marker-ok', { timeout: 20_000 })
    await expect(rows2).toContainText('e2e-after-interrupt', { timeout: 20_000 })
    await expect(pane2.getByTestId('term-status')).toHaveText('live', { timeout: 20_000 })

    // Still live: new input after the reattach reaches the same shell.
    const input2 = pane2.locator('.xterm-helper-textarea')
    await input2.focus()
    await input2.pressSequentially("printf 'e2e-%s\\n' reattached", { delay: 5 })
    await input2.press('Enter')
    await expect(rows2).toContainText('e2e-reattached', { timeout: 20_000 })

    // ── Remote exit: shown, and the terminal stays listed until closed ──
    await input2.pressSequentially('exit 7', { delay: 5 })
    await input2.press('Enter')
    await expect(pane2.getByTestId('term-exited')).toContainText('exit 7', { timeout: 20_000 })
    await expect(tab2).toHaveClass(/exited/)
    const listed = await request.get('/api/plugin-ui/ssh-fleet/terminals', {
      headers: { Authorization: `Bearer ${token}` },
    })
    const { terminals } = (await listed.json()) as {
      terminals: Array<{ host_id: string; exited: { code: number } | null }>
    }
    expect(terminals.filter((t) => t.host_id === host.id)).toHaveLength(1)
    expect(terminals.find((t) => t.host_id === host.id)?.exited?.code).toBe(7)

    // ── Close: an exited terminal closes without a confirmation ─────────
    await pane2.getByTestId('term-close').click()
    await expect(frame2.getByTestId('term-tab')).toHaveCount(0)
    await expect(frame2.getByTestId('tab-activity')).toHaveClass(/active/)
    const after = await request.get('/api/plugin-ui/ssh-fleet/terminals', {
      headers: { Authorization: `Bearer ${token}` },
    })
    const remaining = (await after.json()) as { terminals: Array<{ host_id: string }> }
    expect(remaining.terminals.filter((t) => t.host_id === host.id)).toHaveLength(0)

    // The activity feed recorded the terminal lifecycle.
    const activity = await request.get('/api/plugin-ui/ssh-fleet/activity?host=all&since=0', {
      headers: { Authorization: `Bearer ${token}` },
    })
    const { items } = (await activity.json()) as { items: Array<{ tool: string; summary: string }> }
    expect(items.some((a) => a.tool === 'ssh_terminal' && /opened/.test(a.summary))).toBeTruthy()
    expect(items.some((a) => a.tool === 'ssh_terminal' && /closed/.test(a.summary))).toBeTruthy()
  } finally {
    sshd.child.kill()
  }
})
