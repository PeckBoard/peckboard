import { test, expect, type Page } from '../harness'
import { existsSync, mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import {
  FLEET_WASM,
  SERVER_BIN,
  Server,
  addFleetHost,
  expectScreen,
  freePort,
  login,
  screen,
  spawnSshd,
  which,
} from './ssh-harness'

/**
 * Terminals in saved Views, mixed with sessions: a view holding a session
 * pane plus two terminal panes on the same host gets two separate shells
 * (output isolated, keystrokes only reach the focused pane), the layout and
 * shells survive a reload (tmux keeps a shell variable), and "Close
 * terminal" turns its pane into a reopen placeholder without touching the
 * session pane. Own server + throwaway sshd, like ssh-terminal.spec.
 */

const USER = 'view-term-user'
const PASS = 'view-term-password-1234'

const termPanes = (page: Page) => page.getByTestId('view-terminal-pane')
const paneOf = (page: Page, terminalId: string) =>
  page.locator('[data-testid="view-widget"]', {
    has: page.locator(`[data-terminal-id="${terminalId}"]`),
  })

test('a view mixes a session with two terminals on one host: isolated, focus-driven, persistent, closable', async ({
  request,
  page,
}) => {
  test.setTimeout(180_000)
  test.skip(!existsSync(FLEET_WASM), 'ssh-fleet plugin wasm not built')
  test.skip(!existsSync(SERVER_BIN), 'peckboard binary not built')
  test.skip(!which('tmux'), 'tmux not installed')
  const sshd = await spawnSshd()
  test.skip(!sshd, 'OpenSSH sshd/ssh-keygen not available')
  if (!sshd) return

  const server = new Server(await freePort(), USER, PASS)
  const opened: string[] = []
  let token = ''
  try {
    await server.start()
    token = await login(request, server.base, USER, PASS)
    const auth = { Authorization: `Bearer ${token}` }
    const label = await addFleetHost(request, server.base, token, sshd)
    const hosts = (await (
      await request.get(`${server.base}/api/terminals/hosts`, { headers: auth })
    ).json()) as { id: string; label: string }[]
    const hostId = hosts.find((h) => h.label === label)?.id ?? ''
    expect(hostId).not.toBe('')

    // A session to sit next to the terminals.
    const folder = await request.post(`${server.base}/api/folders`, {
      headers: auth,
      data: { name: 'viewterm', path: mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-vt-')) },
    })
    expect(folder.ok()).toBeTruthy()
    const folderId = ((await folder.json()) as { id: string }).id
    const sess = await request.post(`${server.base}/api/sessions`, {
      headers: auth,
      data: { name: 'beside the shells', folder_id: folderId },
    })
    expect(sess.ok()).toBeTruthy()
    const sessionId = ((await sess.json()) as { id: string }).id
    const created = await request.post(`${server.base}/api/me/views`, {
      headers: auth,
      data: { name: 'Ops', layout: { kind: 'leaf', sessionId } },
    })
    expect(created.ok()).toBeTruthy()
    const viewId = ((await created.json()) as { id: string }).id

    // ── Add two terminals on the same host from the view header ─────────
    await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
    await page.goto(`${server.base}/views/${viewId}`)
    await expect(page.getByTestId('view-editor')).toBeVisible({ timeout: 15_000 })
    for (let i = 1; i <= 2; i++) {
      await page.getByTestId('add-widget-button').click()
      await page.getByTestId('view-add-terminal').click()
      await page.getByTestId(`view-add-terminal-host-${hostId}`).click()
      await expect(termPanes(page)).toHaveCount(i, { timeout: 15_000 })
    }
    const [t1, t2] = await termPanes(page).evaluateAll((els) =>
      els.map((e) => e.getAttribute('data-terminal-id') ?? ''),
    )
    opened.push(t1, t2)
    expect(t1).not.toBe('')
    expect(t2).not.toBe('')
    expect(t1).not.toBe(t2)
    for (const id of [t1, t2]) {
      await expect(
        page.locator(`[data-terminal-id="${id}"] [data-testid="terminal-pane"]`),
      ).toHaveAttribute('data-phase', 'live', { timeout: 20_000 })
    }
    await expect(
      page.locator(`[data-testid="view-widget"][data-pane-id="${sessionId}"]`),
    ).toBeVisible()

    // ── Keystrokes reach only the focused pane; the outline follows ─────
    await page.locator(`[data-terminal-id="${t1}"] .xterm`).click()
    await expect(paneOf(page, t1)).toHaveAttribute('data-focused', 'true')
    await page.keyboard.type('export PV=one; echo first-$((1+1))\n')
    await expectScreen(page, /first-2/, t1)
    await page.locator(`[data-terminal-id="${t2}"] .xterm`).click()
    await expect(paneOf(page, t2)).toHaveAttribute('data-focused', 'true')
    await expect(paneOf(page, t1)).not.toHaveAttribute('data-focused', 'true')
    await page.keyboard.type('echo second-$((2+3))\n')
    await expectScreen(page, /second-5/, t2)
    expect(await screen(page, t2)).not.toMatch(/first-2/)
    expect(await screen(page, t1)).not.toMatch(/second-5/)

    // ── Reload: layout + both shells come back; the variable survived ───
    await expect(page.getByTestId('view-save-state')).toHaveAttribute('data-state', 'saved')
    await page.reload()
    await expect(termPanes(page)).toHaveCount(2, { timeout: 15_000 })
    for (const id of [t1, t2]) {
      await expect(
        page.locator(`[data-terminal-id="${id}"] [data-testid="terminal-pane"]`),
      ).toHaveAttribute('data-phase', 'live', { timeout: 20_000 })
    }
    await expect(
      page.locator(`[data-testid="view-widget"][data-pane-id="${sessionId}"]`),
    ).toBeVisible()
    await page.locator(`[data-terminal-id="${t1}"] .xterm`).click()
    await page.keyboard.type('echo "pv-$PV"\n')
    await expectScreen(page, /pv-one/, t1)

    // ── Close terminal: its pane placeholders, the rest is untouched ────
    await paneOf(page, t2).getByTestId('widget-menu').click()
    await page.getByTestId('view-terminal-close').click()
    const confirm = page.getByTestId('view-terminal-close-confirm')
    await expect(confirm).toBeVisible()
    await confirm.getByTestId('confirm-dialog-confirm').click()
    const closed = page.getByTestId('view-terminal-closed')
    await expect(closed).toHaveCount(1)
    await expect(closed.getByTestId('view-terminal-reopen')).toContainText('Reopen on')
    await expect(termPanes(page)).toHaveCount(1)
    await expect(
      page.locator(`[data-testid="view-widget"][data-pane-id="${sessionId}"]`),
    ).toBeVisible()
    await expect(
      page.locator(`[data-terminal-id="${t1}"] [data-testid="terminal-pane"]`),
    ).toHaveAttribute('data-phase', 'live')
    // Still a placeholder after a reload (the server reports it closed).
    await page.reload()
    await expect(page.getByTestId('view-terminal-closed')).toHaveCount(1, { timeout: 15_000 })
    await expect(termPanes(page)).toHaveCount(1)
  } finally {
    if (token && server.child) {
      for (const id of opened) {
        await request
          .delete(`${server.base}/api/terminals/${id}`, {
            headers: { Authorization: `Bearer ${token}` },
          })
          .catch(() => {})
      }
      await new Promise((r) => setTimeout(r, 1000))
    }
    await server.stop()
    sshd.child.kill()
  }
})
