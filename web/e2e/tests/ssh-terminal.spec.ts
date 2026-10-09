import { test, expect } from '../harness'
import { existsSync } from 'node:fs'
import {
  FLEET_WASM,
  SERVER_BIN,
  Server,
  addFleetHost,
  expectScreen,
  freePort,
  login,
  spawnSshd,
  which,
} from './ssh-harness'

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

  const server = new Server(await freePort(), USER, PASS)
  let terminalId = ''
  let token = ''
  try {
    await server.start()
    token = await login(request, server.base, USER, PASS)
    const label = await addFleetHost(request, server.base, token, sshd)

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
