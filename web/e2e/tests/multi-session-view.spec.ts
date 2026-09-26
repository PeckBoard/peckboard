import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Multi-session split view: the session view's auto subagent panes (both
 * Claude-native Agent subagents and Peckboard spawn_subagent children) and
 * saved multi-session Views (create, rearrange, resize, persist, narrow).
 *
 * Deterministic via `mock:subagent-native` (native Agent frames tagged with
 * `parentToolUseId`) and `mock:subagent` (a spawn_subagent tool card whose
 * output names a real child session — the message text is the child id).
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type Auth = { token: string; auth: Record<string, string> }

async function authenticate(request: APIRequestContext): Promise<Auth> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function createFolder(request: APIRequestContext, auth: Auth, name: string) {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-${name}-`))
  const res = await request.post('/api/folders', {
    headers: auth.auth,
    data: { name, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function createSession(
  request: APIRequestContext,
  auth: Auth,
  folderId: string,
  name: string,
): Promise<string> {
  const res = await request.post('/api/sessions', {
    headers: auth.auth,
    data: { name, folder_id: folderId },
  })
  expect(res.ok(), `create session failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function send(
  request: APIRequestContext,
  auth: Auth,
  sessionId: string,
  text: string,
  model: string,
) {
  const res = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth.auth,
    data: { text, model },
  })
  expect(res.ok(), `send failed: ${await res.text()}`).toBeTruthy()
}

async function loadAt(page: Page, token: string, route: string, paneMode?: 'off') {
  await page.addInitScript(
    ({ t, mode }) => {
      localStorage.setItem('peckboard_token', t)
      if (mode) localStorage.setItem('peckboard.subagentPanes', mode)
    },
    { t: token, mode: paneMode ?? null },
  )
  await page.goto(route)
}

/** Open a session and wait until its WS subscription is on the wire. A cold
 *  page load backfills over HTTP while the socket is still authenticating, so
 *  a turn sent in between would be missed by the live feed. */
async function openSession(page: Page, token: string, sessionId: string) {
  const subscribed = new Promise<void>((resolve) => {
    page.on('websocket', (ws) => {
      ws.on('framesent', (frame) => {
        const text = typeof frame.payload === 'string' ? frame.payload : ''
        if (text.includes('"subscribe"') && text.includes(sessionId)) resolve()
      })
    })
  })
  await loadAt(page, token, `/sessions/${sessionId}`)
  await subscribed
  await expect(page.getByTestId('session-workspace')).toBeVisible({ timeout: 15_000 })
  // Let the server register the subscription before a turn starts.
  await page.waitForTimeout(300)
}

test.describe('session view subagent panes', () => {
  test('a finished native Agent subagent leaves Auto; overflow reopens it', async ({
    request,
    page,
  }) => {
    const auth = await authenticate(request)
    const folder = await createFolder(request, auth, 'multiview-native')
    const parent = await createSession(request, auth, folder, 'native parent')

    await openSession(page, auth.token, parent)
    const workspace = page.getByTestId('session-workspace')
    // A lone session renders bare: no pane chrome yet.
    await expect(workspace.getByTestId('split-pane-header')).toHaveCount(0)

    await send(request, auth, parent, 'explore', 'mock:subagent-native')

    // Auto shows running subagents only: once the child finishes, no pane.
    await expect(workspace).toContainText('Parent done', { timeout: 15_000 })
    const pane = page.getByTestId('native-subagent-pane')
    await expect(pane).toHaveCount(0)

    await page.getByTestId('subagent-overflow-chip').click()
    await page.getByTestId('subagent-overflow-item').click()
    await expect(pane).toBeVisible()
    await expect(pane).toContainText('Child is looking around')
    await expect(pane).toContainText('Finished')

    const primary = page.locator('[data-testid="split-pane"][data-pane-id="@primary"]')
    await expect(primary).not.toContainText('Child is looking around')
    const nativePane = page.getByTestId('split-pane').filter({ has: pane })
    await expect(nativePane.getByTestId('split-pane-header')).toContainText('Explore repo')
    await expect(nativePane.getByTestId('split-pane-badge')).toHaveText('Done')
  })

  test('a spawn_subagent child pane slides out of Auto when it finishes', async ({
    request,
    page,
  }) => {
    const auth = await authenticate(request)
    const folder = await createFolder(request, auth, 'multiview-finish')
    const child = await createSession(request, auth, folder, 'finish child')
    const parent = await createSession(request, auth, folder, 'finish parent')

    await openSession(page, auth.token, parent)
    const workspace = page.getByTestId('session-workspace')
    const paneChild = workspace.locator(`[data-pane-id="${child}"]`)

    await send(request, auth, parent, child, 'mock:subagent')
    await expect(paneChild).toBeVisible({ timeout: 15_000 })

    // The child runs one turn and ends: its pane leaves the auto view.
    await send(request, auth, child, 'hello', 'mock:happy-path')
    await expect(paneChild).toHaveCount(0, { timeout: 15_000 })
    await expect(workspace.getByTestId('split-pane')).toHaveCount(1)
    await expect(page.getByTestId('subagent-overflow-chip')).toHaveText('+1')
  })

  test('each spawn_subagent child gets a pane; toggle Off hides them', async ({
    request,
    page,
  }) => {
    const auth = await authenticate(request)
    const folder = await createFolder(request, auth, 'multiview-spawn')
    const childA = await createSession(request, auth, folder, 'child alpha')
    const childB = await createSession(request, auth, folder, 'child beta')
    const parent = await createSession(request, auth, folder, 'spawn parent')

    await openSession(page, auth.token, parent)
    const workspace = page.getByTestId('session-workspace')
    const panes = workspace.getByTestId('split-pane')

    await send(request, auth, parent, childA, 'mock:subagent')
    await expect(panes).toHaveCount(2, { timeout: 15_000 })
    await expect(workspace.locator(`[data-pane-id="${childA}"]`)).toBeVisible()
    await expect(
      workspace.locator(`[data-pane-id="${childA}"] [data-testid="split-pane-header"]`),
    ).toContainText('child alpha')
    await expect(page.locator('[data-pane-id="@primary"]')).toContainText('Subagent spawned.', {
      timeout: 15_000,
    })

    await send(request, auth, parent, childB, 'mock:subagent')
    await expect(panes).toHaveCount(3, { timeout: 15_000 })
    await expect(workspace.locator(`[data-pane-id="${childB}"]`)).toBeVisible()
    await expect(workspace.getByTestId('split-divider')).toHaveCount(2)
    // Reopen: the panes slide in after the parent feed has pinned to the
    // bottom at full size; shrinking it must keep the newest turn in view.
    await page.setViewportSize({ width: 1280, height: 560 })
    await page.reload()
    await expect(panes).toHaveCount(3, { timeout: 15_000 })
    await expect(
      page.locator('[data-pane-id="@primary"]').getByText('Subagent spawned.').last(),
    ).toBeInViewport({ timeout: 5_000 })

    // Toggle Off: back to the bare parent view, preference survives reload.
    const toggle = page.getByTestId('subagent-panes-toggle')
    await expect(toggle).toHaveAttribute('data-mode', 'auto')
    await toggle.click()
    await expect(toggle).toHaveAttribute('data-mode', 'off')
    await expect(panes).toHaveCount(1)
    await expect(workspace.getByTestId('split-pane-header')).toHaveCount(0)

    await page.reload()
    await expect(page.getByTestId('subagent-panes-toggle')).toHaveAttribute('data-mode', 'off', {
      timeout: 15_000,
    })
    await expect(workspace.getByTestId('split-pane')).toHaveCount(1)
  })

  test('a closed subagent pane stays closed in Auto, across reloads', async ({ request, page }) => {
    const auth = await authenticate(request)
    const folder = await createFolder(request, auth, 'multiview-closed')
    const childA = await createSession(request, auth, folder, 'closed alpha')
    const childB = await createSession(request, auth, folder, 'closed beta')
    const parent = await createSession(request, auth, folder, 'closed parent')

    await openSession(page, auth.token, parent)
    const workspace = page.getByTestId('session-workspace')
    const panes = workspace.getByTestId('split-pane')
    const paneA = workspace.locator(`[data-pane-id="${childA}"]`)

    await send(request, auth, parent, childA, 'mock:subagent')
    await expect(paneA).toBeVisible({ timeout: 15_000 })
    await paneA.getByTestId('split-pane-close').click()
    await expect(paneA).toHaveCount(0)
    await expect(page.getByTestId('subagent-panes-toggle')).toHaveAttribute('data-mode', 'auto')

    // A new subagent still auto-opens; the closed one does not come back.
    await send(request, auth, parent, childB, 'mock:subagent')
    await expect(workspace.locator(`[data-pane-id="${childB}"]`)).toBeVisible({
      timeout: 15_000,
    })
    await expect(panes).toHaveCount(2)
    await expect(paneA).toHaveCount(0)

    await page.reload()
    await expect(workspace.locator(`[data-pane-id="${childB}"]`)).toBeVisible({
      timeout: 15_000,
    })
    await expect(panes).toHaveCount(2)
    await expect(paneA).toHaveCount(0)

    // Explicitly reopening it from the overflow chip brings it back.
    await page.getByTestId('subagent-overflow-chip').click()
    await page.getByTestId('subagent-overflow-item').click()
    await expect(paneA).toBeVisible()
    await expect(panes).toHaveCount(3)
  })
})

test.describe('saved multi-session views', () => {
  async function paneBox(page: Page, sessionId: string) {
    const box = await page
      .locator(`[data-testid="view-split-layout"] [data-pane-id="${sessionId}"]`)
      .boundingBox()
    expect(box, `pane ${sessionId} has a box`).toBeTruthy()
    return box!
  }

  async function viewLayout(request: APIRequestContext, auth: Auth, viewId: string) {
    const res = await request.get(`/api/me/views/${viewId}`, { headers: auth.auth })
    expect(res.ok()).toBeTruthy()
    return JSON.stringify(((await res.json()) as { layout: unknown }).layout)
  }

  test('create, rearrange, resize and persist a view', async ({ request, page }) => {
    await page.setViewportSize({ width: 1400, height: 900 })
    const auth = await authenticate(request)
    const folder = await createFolder(request, auth, 'multiview-view')
    const tag = `mv${Date.now()}`
    const ids = [
      await createSession(request, auth, folder, `${tag} one`),
      await createSession(request, auth, folder, `${tag} two`),
      await createSession(request, auth, folder, `${tag} three`),
    ]

    await loadAt(page, auth.token, '/')
    await page.getByTestId('rail-views').click()
    await page.getByTestId('views-new').click()
    const modal = page.getByTestId('new-view-modal')
    await expect(modal).toBeVisible()
    await modal.getByTestId('new-view-name').fill(`${tag} view`)
    await modal.getByTestId('new-view-session-search').fill(tag)
    for (const id of ids) {
      await modal
        .locator(`[data-testid="new-view-session-option"][data-session-id="${id}"]`)
        .click()
    }
    await modal.getByTestId('new-view-starter').selectOption('columns')
    await modal.getByTestId('new-view-submit').click()

    const editor = page.getByTestId('view-editor')
    await expect(editor).toBeVisible({ timeout: 15_000 })
    const viewId = (await editor.getAttribute('data-view-id'))!
    await expect(page).toHaveURL(new RegExp(`/views/${viewId}`))
    await expect(editor.getByTestId('split-pane')).toHaveCount(3)
    await expect(editor.getByTestId('split-divider')).toHaveCount(2)

    // Columns: all three share a row.
    const a0 = await paneBox(page, ids[0])
    const c0 = await paneBox(page, ids[2])
    expect(Math.abs(a0.y - c0.y)).toBeLessThan(2)
    const before = await viewLayout(request, auth, viewId)

    // Drag pane three's header onto pane one's bottom edge → one/three stack.
    const header = editor.locator(`[data-pane-id="${ids[2]}"] [data-testid="split-pane-header"]`)
    const hb = (await header.boundingBox())!
    await page.mouse.move(hb.x + 40, hb.y + hb.height / 2)
    await page.mouse.down()
    await page.mouse.move(hb.x + 60, hb.y + hb.height / 2 + 10, { steps: 4 })
    const zone = editor.locator(`[data-pane-id="${ids[0]}"] [data-testid="split-drop-zone-bottom"]`)
    await expect(zone).toBeAttached()
    const zb = (await zone.boundingBox())!
    await page.mouse.move(zb.x + zb.width / 2, zb.y + zb.height / 2, { steps: 8 })
    await page.mouse.up()

    const save = editor.getByTestId('view-save-state')
    await expect(save).toHaveAttribute('data-state', 'saved', { timeout: 10_000 })
    await expect.poll(() => viewLayout(request, auth, viewId)).not.toBe(before)
    const a1 = await paneBox(page, ids[0])
    const c1 = await paneBox(page, ids[2])
    expect(c1.y).toBeGreaterThan(a1.y + a1.height / 2)
    expect(Math.abs(c1.x - a1.x)).toBeLessThan(2)

    await page.reload()
    await expect(page.getByTestId('view-editor')).toBeVisible({ timeout: 15_000 })
    await expect(editor.getByTestId('split-pane')).toHaveCount(3)
    const a2 = await paneBox(page, ids[0])
    const c2 = await paneBox(page, ids[2])
    expect(c2.y).toBeGreaterThan(a2.y + a2.height / 2)
    expect(Math.abs(c2.x - a2.x)).toBeLessThan(2)

    // Keyboard-resize the column divider; the new ratio persists.
    const divider = editor.locator('[data-testid="split-divider"][data-dir="row"]').first()
    const startValue = Number(await divider.getAttribute('aria-valuenow'))
    await divider.focus()
    for (let i = 0; i < 5; i++) await page.keyboard.press('ArrowRight')
    await expect
      .poll(async () => Number(await divider.getAttribute('aria-valuenow')))
      .toBeGreaterThan(startValue + 5)
    const resized = Number(await divider.getAttribute('aria-valuenow'))
    await expect(save).toHaveAttribute('data-state', 'saved', { timeout: 10_000 })

    await page.reload()
    await expect(page.getByTestId('view-editor')).toBeVisible({ timeout: 15_000 })
    await expect(
      editor.locator('[data-testid="split-divider"][data-dir="row"]').first(),
    ).toHaveAttribute('aria-valuenow', String(resized))
  })

  test('+ ▾ New split view creates a view and adds sessions to it', async ({ request, page }) => {
    await page.setViewportSize({ width: 1400, height: 900 })
    const auth = await authenticate(request)
    const folder = await createFolder(request, auth, 'multiview-plus')
    const tag = `plus${Date.now()}`
    const one = await createSession(request, auth, folder, `${tag} one`)
    const two = await createSession(request, auth, folder, `${tag} two`)

    await loadAt(page, auth.token, '/')
    await page.getByTestId('tab-new-more').click()
    await page.getByTestId('tab-new-menu-view').click()
    const modal = page.getByTestId('new-view-modal')
    await expect(modal).toBeVisible()
    await modal.getByTestId('new-view-name').fill(`${tag} view`)
    await modal.getByTestId('new-view-session-search').fill(tag)
    await modal.locator(`[data-testid="new-view-session-option"][data-session-id="${one}"]`).click()
    await modal.getByTestId('new-view-submit').click()

    const editor = page.getByTestId('view-editor')
    await expect(editor).toBeVisible({ timeout: 15_000 })
    await expect(page).toHaveURL(/\/views\/[^/]+$/)
    await expect(editor.locator(`[data-pane-id="${one}"]`)).toBeVisible()

    await editor.getByTestId('view-add-session').click()
    await page.getByTestId('view-add-session-search').fill(`${tag} two`)
    await page.getByRole('option', { name: `${tag} two` }).click()
    await expect(editor.locator(`[data-pane-id="${two}"]`)).toBeVisible()
    await expect(editor.getByTestId('split-pane')).toHaveCount(2)
  })

  test('narrow viewport shows a pane switcher instead of dividers', async ({ request, page }) => {
    await page.setViewportSize({ width: 760, height: 900 })
    const auth = await authenticate(request)
    const folder = await createFolder(request, auth, 'multiview-narrow')
    const ids = [
      await createSession(request, auth, folder, 'narrow one'),
      await createSession(request, auth, folder, 'narrow two'),
    ]
    const res = await request.post('/api/me/views', {
      headers: auth.auth,
      data: {
        name: 'narrow view',
        layout: {
          kind: 'split',
          dir: 'row',
          ratios: [0.5, 0.5],
          children: ids.map((sessionId) => ({ kind: 'leaf', sessionId })),
        },
      },
    })
    expect(res.status(), await res.text()).toBe(201)
    const viewId = ((await res.json()) as { id: string }).id

    await loadAt(page, auth.token, `/views/${viewId}`)
    const editor = page.getByTestId('view-editor')
    await expect(editor).toBeVisible({ timeout: 15_000 })
    const tabs = editor.getByTestId('split-switcher-tab')
    await expect(tabs).toHaveCount(2)
    await expect(editor.getByTestId('split-divider')).toHaveCount(0)

    // Only the selected pane is on screen; switching tabs swaps it.
    const visiblePanes = editor.locator('[data-testid="split-pane"]:not([aria-hidden="true"])')
    await expect(visiblePanes).toHaveCount(1)
    await tabs.nth(1).click()
    await expect(tabs.nth(1)).toHaveAttribute('aria-selected', 'true')
    await expect(visiblePanes).toHaveAttribute('data-pane-id', ids[1])
  })
})
