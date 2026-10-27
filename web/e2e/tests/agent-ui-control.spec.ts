import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * An agent drives the user's open browser tab and edits their views.
 *
 * A chat session runs on `mock:mcp`, which executes every ```mcp block in a
 * message against the REAL MCP handler — so `ui_open` / `create_view` /
 * `add_view_widget` exercise name resolution, the per-user `ui-navigate` /
 * `views-changed` WS frames, and the browser's reaction. The page is opened
 * separately (it is the user's tab); the agent is driven over the API.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type Auth = Record<string, string>

async function setup(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  const auth: Auth = { Authorization: `Bearer ${token}` }
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-uictl-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `uictl-${path.basename(folderPath)}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folderId = ((await folderRes.json()) as { id: string }).id
  return { token, auth, folderId }
}

async function createSession(
  request: APIRequestContext,
  auth: Auth,
  folderId: string,
  name: string,
  model?: string,
): Promise<string> {
  const res = await request.post('/api/sessions', {
    headers: auth,
    data: { name, folder_id: folderId, ...(model ? { model } : {}) },
  })
  expect(res.ok(), `create session failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

/** Have the agent session call `tool` (one ```mcp block per message). */
async function agentCalls(
  request: APIRequestContext,
  auth: Auth,
  sessionId: string,
  tool: string,
  args: Record<string, unknown>,
) {
  const text = '```mcp\n' + JSON.stringify({ tool, args }) + '\n```'
  const res = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth,
    data: { text, model: 'mock:mcp' },
  })
  expect(res.ok(), `send message failed: ${await res.text()}`).toBeTruthy()
}

async function openApp(page: Page, token: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto('/')
}

test('ui_open and the view tools drive the user’s open tab', async ({ request, page }) => {
  test.setTimeout(120_000)
  const { token, auth, folderId } = await setup(request)
  const suffix = Date.now().toString(36)
  const targetName = `Target ${suffix}`
  const agentName = `Agent ${suffix}`
  const targetId = await createSession(request, auth, folderId, targetName)
  const agentId = await createSession(request, auth, folderId, agentName, 'mock:mcp')

  await openApp(page, token)
  const startPath = new URL(page.url()).pathname

  // 1. ui_open by fuzzy name. The first call retries until this tab's
  //    socket is registered (the server only targets live connections).
  await expect
    .poll(
      async () => {
        await agentCalls(request, auth, agentId, 'ui_open', {
          target: 'session',
          name: `target ${suffix}`,
        })
        return page.waitForURL(new RegExp(`/sessions/${targetId}$`), { timeout: 4_000 }).then(
          () => true,
          () => false,
        )
      },
      { timeout: 40_000, intervals: [0] },
    )
    .toBe(true)
  const toast = page.getByTestId('agent-nav-toast')
  await expect(toast).toBeVisible()
  await expect(toast).toContainText(`${agentName} opened ${targetName}`)

  // Back undoes the jump.
  await page.getByTestId('agent-nav-toast-action').click()
  await expect(page).toHaveURL(new RegExp(`${startPath === '/' ? '/$' : startPath}`))
  await expect(toast).toHaveCount(0)

  // 2. create_view with show=true lands this tab on the new view.
  await page.evaluate(() => {
    ;(window as unknown as { __sameDocument: boolean }).__sameDocument = true
  })
  const viewName = `Ops ${suffix}`
  await agentCalls(request, auth, agentId, 'create_view', {
    name: viewName,
    show: true,
    widgets: [{ id: 'w-first', kind: 'note', body: 'first note from agent' }],
  })
  await expect(page).toHaveURL(/\/views\/[^/]+$/, { timeout: 20_000 })
  const viewId = new URL(page.url()).pathname.split('/').pop()!
  await expect(page.getByTestId('view-editor')).toHaveAttribute('data-view-id', viewId)
  await expect(page.getByText('first note from agent')).toBeVisible({ timeout: 15_000 })
  await expect(toast).toContainText(`${agentName} opened ${viewName}`)

  // 3. add_view_widget shows up live — no reload.
  await agentCalls(request, auth, agentId, 'add_view_widget', {
    id: viewId,
    widget: { id: 'w-second', kind: 'note', body: 'second note from agent' },
  })
  await expect(page.getByText('second note from agent')).toBeVisible({ timeout: 20_000 })
  await expect(page.getByTestId('view-widget')).toHaveCount(2)
  expect(
    await page.evaluate(() => (window as unknown as { __sameDocument?: boolean }).__sameDocument),
    'the view refreshed in place, not by a page reload',
  ).toBe(true)

  // 4. A local edit still being saved + an agent edit → "Changed elsewhere",
  //    and nothing is silently overwritten until the user picks.
  let releaseSave: () => void = () => {}
  await page.route(`**/api/me/views/${viewId}`, async (route) => {
    if (route.request().method() !== 'PUT') return route.continue()
    await new Promise<void>((resolve) => (releaseSave = resolve))
    await route.abort().catch(() => {})
  })
  await page
    .locator('[data-testid="view-widget"][data-widget-id="w-first"]')
    .getByTestId('widget-menu')
    .click()
  await page.getByTestId('widget-remove').click()
  await expect(page.getByTestId('view-widget')).toHaveCount(1)
  await expect(page.getByTestId('view-save-state')).toHaveAttribute('data-state', 'saving')
  // Let the debounce fire so the (held) save is in flight.
  await page.waitForTimeout(800)
  await agentCalls(request, auth, agentId, 'add_view_widget', {
    id: viewId,
    widget: { id: 'w-third', kind: 'note', body: 'third note from agent' },
  })
  const notice = page.getByTestId('view-stale-notice')
  await expect(notice).toBeVisible({ timeout: 20_000 })
  await expect(notice).toContainText('Changed elsewhere')
  await expect(page.getByText('third note from agent')).toHaveCount(0)

  // Reload shows the server's version (the agent's widgets; the local
  // removal never landed). The fetch waits for the save still on the wire,
  // so it can't be overwritten by that save landing afterwards.
  await page.getByTestId('view-stale-reload').click()
  await expect(notice).toHaveCount(0)
  await page.waitForTimeout(500)
  await expect(page.getByText('third note from agent')).toHaveCount(0)
  releaseSave()
  await expect(page.getByText('third note from agent')).toBeVisible({ timeout: 15_000 })
  await expect(page.getByTestId('view-widget')).toHaveCount(3)
  await page.unrouteAll({ behavior: 'ignoreErrors' })

  await request.delete(`/api/me/views/${viewId}`, { headers: auth })
  await request.delete(`/api/sessions/${agentId}`, { headers: auth })
  await request.delete(`/api/sessions/${targetId}`, { headers: auth })
})
