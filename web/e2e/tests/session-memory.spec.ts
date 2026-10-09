import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * End-to-end for the per-session memory pool.
 *
 * The agent (driven by `mock:mcp`, which runs every ```mcp block in the
 * prompt against the REAL MCP handler) saves a note with `memory_add`. We
 * then verify the user-facing surface: the 3-dot menu's "Memory" item opens
 * a modal listing the note; "Clear session" wipes the transcript but NOT the
 * note; deleting from the modal (with confirm) removes it for good.
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

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

function mcpBlock(tool: string, args: Record<string, unknown>): string {
  return '```mcp\n' + JSON.stringify({ tool, args }) + '\n```'
}

type Memory = { id: string; content: string }

async function listMemories(
  request: APIRequestContext,
  auth: Record<string, string>,
  sessionId: string,
): Promise<Memory[]> {
  const res = await request.get(`/api/sessions/${sessionId}/memories`, { headers: auth })
  expect(res.ok(), `list memories failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { memories: Memory[] }).memories
}

test('agent memory survives Clear session and can be deleted from the Memory modal', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-memory-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-memory-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'memory flow', folder_id: folder.id, model: 'mock:mcp' },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  // 1. The agent saves a note through the real memory_add handler.
  const note = 'User prefers tabs over spaces in this repo'
  const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
    headers: auth,
    data: {
      text: `remember this\n${mcpBlock('memory_add', { content: note })}`,
      model: 'mock:mcp',
    },
  })
  expect(sendRes.ok(), `send message failed: ${await sendRes.text()}`).toBeTruthy()
  await expect
    .poll(async () => (await listMemories(request, auth, session.id)).length, {
      timeout: 15_000,
    })
    .toBe(1)

  // 2. Memory is reachable from the chat 3-dot menu and lists the note.
  await loadAt(page, token, `/sessions/${session.id}`)
  await expect(page.locator('.chat-toolbar')).toBeVisible({ timeout: 10_000 })
  await page.getByTestId('chat-toolbar-menu').click()
  await page.getByTestId('chat-menu-memory').click()
  const modal = page.getByTestId('session-memory-modal')
  await expect(modal).toBeVisible()
  await expect(modal.getByTestId('session-memory-row')).toHaveCount(1)
  await expect(modal.getByTestId('session-memory-row')).toContainText(note)
  await page.keyboard.press('Escape')
  await expect(modal).toHaveCount(0)

  // 3. Clear the session from the same menu: transcript gone, memory kept.
  await page.getByTestId('chat-toolbar-menu').click()
  await page.getByTestId('chat-menu-clear').click()
  const clearDialog = page.getByTestId('confirm-clear')
  await expect(clearDialog).toBeVisible()
  await clearDialog.getByTestId('confirm-dialog-confirm').click()
  await expect(clearDialog).toHaveCount(0)
  await expect
    .poll(async () => {
      const res = await request.get(`/api/sessions/${session.id}/events`, { headers: auth })
      const body = (await res.json()) as { events?: unknown[] } | unknown[]
      return Array.isArray(body) ? body.length : (body.events?.length ?? 0)
    })
    .toBe(0)
  expect((await listMemories(request, auth, session.id)).map((m) => m.content)).toEqual([note])

  // 4. The tab-strip context menu carries the same "Memory" entry.
  const tab = page.locator('.tab-opened.tab-active')
  await expect(tab).toBeVisible()
  await tab.click({ button: 'right' })
  await page.locator('.context-menu button', { hasText: /^Memory$/ }).click()
  await expect(modal).toBeVisible()
  await expect(modal.getByTestId('session-memory-row')).toHaveCount(1)

  // 5. Delete from the modal: confirm dialog, then the row and the API
  //    entry are gone.
  await modal.getByTestId('session-memory-delete').click()
  const deleteDialog = page.getByTestId('confirm-delete-memory')
  await expect(deleteDialog).toBeVisible()
  await deleteDialog.getByTestId('confirm-dialog-confirm').click()
  await expect(deleteDialog).toHaveCount(0)
  await expect(modal.getByTestId('session-memory-row')).toHaveCount(0)
  await expect(modal.getByTestId('session-memory-empty')).toBeVisible()
  expect(await listMemories(request, auth, session.id)).toEqual([])

  // 6. Clean up: deleting the session drops its pool (FK cascade).
  await page.keyboard.press('Escape')
  const del = await request.delete(`/api/sessions/${session.id}`, { headers: auth })
  expect(del.ok(), `delete session failed: ${await del.text()}`).toBeTruthy()
})
