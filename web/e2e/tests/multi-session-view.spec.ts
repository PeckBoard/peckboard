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
  test('a finished native Agent subagent leaves Auto; its tool card reopens it', async ({
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
    // Finished subagents are not listed in the overflow chip (active only);
    // the Agent tool card's "Show pane" still reopens it.
    await expect(page.getByTestId('subagent-overflow-chip')).toHaveCount(0)
    await workspace.getByTestId('tool-show-pane').first().click()
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
    await expect(page.getByTestId('subagent-overflow-chip')).toHaveCount(0)
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
  type Rect = { x: number; y: number; w: number; h: number }
  type Widget = Rect & { id: string; kind: string; sessionId?: string | null }

  const widgetOf = (page: Page, sessionId: string) =>
    page.locator(`[data-testid="view-widget"][data-pane-id="${sessionId}"]`)

  async function viewWidgets(request: APIRequestContext, auth: Auth, viewId: string) {
    const res = await request.get(`/api/me/views/${viewId}`, { headers: auth.auth })
    expect(res.ok()).toBeTruthy()
    return ((await res.json()) as { widgets: Widget[] }).widgets
  }

  async function rectOf(
    request: APIRequestContext,
    auth: Auth,
    viewId: string,
    sessionId: string,
  ): Promise<Rect | undefined> {
    const w = (await viewWidgets(request, auth, viewId)).find((x) => x.sessionId === sessionId)
    return w && { x: w.x, y: w.y, w: w.w, h: w.h }
  }

  /** Press at `from`, travel to `to` in small steps, release. */
  async function pointerDrag(
    page: Page,
    from: { x: number; y: number },
    to: { x: number; y: number },
  ) {
    await page.mouse.move(from.x, from.y)
    await page.mouse.down()
    await page.mouse.move(from.x + 8, from.y + 8, { steps: 2 })
    await page.mouse.move(to.x, to.y, { steps: 12 })
    await page.mouse.up()
  }

  test('create, drag, resize and persist a view', async ({ request, page }) => {
    await page.setViewportSize({ width: 1400, height: 1100 })
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
    await expect(editor.getByTestId('view-widget-grid')).toHaveAttribute('data-mode', 'grid')
    await expect(editor.getByTestId('view-widget')).toHaveCount(3)

    // Columns starter: three equal columns on one row.
    expect(await rectOf(request, auth, viewId, ids[0])).toEqual({ x: 0, y: 0, w: 4, h: 16 })
    expect(await rectOf(request, auth, viewId, ids[2])).toEqual({ x: 8, y: 0, w: 4, h: 16 })

    // Drag widget three by its header onto widget one's spot: three takes
    // the top-left cell and one is pushed down below it.
    const header = widgetOf(page, ids[2]).getByTestId('widget-drag-handle')
    const hb = (await header.boundingBox())!
    const ob = (await widgetOf(page, ids[0]).getByTestId('widget-drag-handle').boundingBox())!
    await pointerDrag(
      page,
      { x: hb.x + 10, y: hb.y + hb.height / 2 },
      { x: ob.x + 10, y: ob.y + ob.height / 2 },
    )

    const save = editor.getByTestId('view-save-state')
    await expect(save).toHaveAttribute('data-state', 'saved', { timeout: 10_000 })
    await expect
      .poll(() => rectOf(request, auth, viewId, ids[2]))
      .toEqual({ x: 0, y: 0, w: 4, h: 16 })
    expect(await rectOf(request, auth, viewId, ids[0])).toEqual({ x: 0, y: 16, w: 4, h: 16 })

    await page.reload()
    await expect(page.getByTestId('view-editor')).toBeVisible({ timeout: 15_000 })
    await expect(editor.getByTestId('view-widget')).toHaveCount(3)
    const one = (await widgetOf(page, ids[0]).boundingBox())!
    const three = (await widgetOf(page, ids[2]).boundingBox())!
    expect(one.y).toBeGreaterThan(three.y + three.height / 2)
    expect(Math.abs(one.x - three.x)).toBeLessThan(2)

    // Resize widget two from its bottom-right handle: two columns wider,
    // six rows shorter.
    const before = (await rectOf(request, auth, viewId, ids[1]))!
    expect(before).toEqual({ x: 4, y: 0, w: 4, h: 16 })
    const twoBox = (await widgetOf(page, ids[1]).boundingBox())!
    const colStep = twoBox.width / 4 + 2 // (4·colW + 3·gap) / 4 ≈ colW + gap
    const handle = page
      .locator(`.widget-cell:has([data-pane-id="${ids[1]}"])`)
      .getByTestId('widget-resize-handle')
    const rb = (await handle.boundingBox())!
    const start = { x: rb.x + rb.width / 2, y: rb.y + rb.height / 2 }
    await pointerDrag(page, start, { x: start.x + 2 * colStep, y: start.y - 6 * 48 })

    await expect(save).toHaveAttribute('data-state', 'saved', { timeout: 10_000 })
    await expect
      .poll(() => rectOf(request, auth, viewId, ids[1]))
      .toEqual({ x: 4, y: 0, w: 6, h: 10 })

    await page.reload()
    await expect(page.getByTestId('view-editor')).toBeVisible({ timeout: 15_000 })
    const resized = (await widgetOf(page, ids[1]).boundingBox())!
    expect(resized.width).toBeGreaterThan(twoBox.width * 1.3)
    expect(resized.height).toBeLessThan(twoBox.height * 0.75)
    expect(await rectOf(request, auth, viewId, ids[1])).toEqual({ x: 4, y: 0, w: 6, h: 10 })
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
    await expect(widgetOf(page, one)).toBeVisible()

    await editor.getByTestId('add-widget-button').click()
    await page.getByTestId('view-add-session').click()
    await page.getByTestId('view-add-session-search').fill(`${tag} two`)
    await page.getByRole('option', { name: `${tag} two` }).click()
    await expect(widgetOf(page, two)).toBeVisible()
    await expect(editor.getByTestId('view-widget')).toHaveCount(2)

    // The added widget's composer sizes to its content, not the autosize cap.
    const composer = widgetOf(page, two).locator('.input-textarea')
    await expect(composer).toBeVisible()
    await expect
      .poll(async () => (await composer.boundingBox())!.height, { timeout: 5_000 })
      .toBeLessThan(60)
  })

  test('narrow viewport stacks widgets in one column without drag', async ({ request, page }) => {
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
        widgets: ids.map((sessionId, i) => ({
          id: `w-narrow-${i}`,
          kind: 'session',
          x: i * 6,
          y: 0,
          w: 6,
          h: 10,
          sessionId,
        })),
      },
    })
    expect(res.status(), await res.text()).toBe(201)
    const viewId = ((await res.json()) as { id: string }).id

    await loadAt(page, auth.token, `/views/${viewId}`)
    const editor = page.getByTestId('view-editor')
    await expect(editor).toBeVisible({ timeout: 15_000 })
    await expect(editor.getByTestId('view-widget-grid')).toHaveAttribute('data-mode', 'narrow')
    await expect(editor.getByTestId('view-widget')).toHaveCount(2)
    await expect(editor.getByTestId('widget-resize-handle')).toHaveCount(0)

    // Side-by-side on desktop, stacked full-width here.
    const a = (await widgetOf(page, ids[0]).boundingBox())!
    const b = (await widgetOf(page, ids[1]).boundingBox())!
    expect(Math.abs(a.x - b.x)).toBeLessThan(2)
    expect(b.y).toBeGreaterThan(a.y + a.height - 2)
  })
})
