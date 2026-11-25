import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

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

// Screenshots for a visual check of the dialog and the setting (light,
// dark, narrow). Written next to the repo; not asserted on.
const SHOTS = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  '../../../.tmp-memory-shots',
)

async function shoot(page: Page, name: string, target: ReturnType<Page['getByTestId']>) {
  for (const scheme of ['light', 'dark'] as const) {
    await page.emulateMedia({ colorScheme: scheme })
    await target.screenshot({ path: path.join(SHOTS, `${name}-${scheme}.png`) })
  }
  await page.emulateMedia({ colorScheme: null })
}

async function makeFolder(request: APIRequestContext, auth: Record<string, string>) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-memory-'))
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-memory-${Date.now()}`, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function makeSession(
  request: APIRequestContext,
  auth: Record<string, string>,
  folderId: string,
  name: string,
) {
  const res = await request.post('/api/sessions', {
    headers: auth,
    data: { name, folder_id: folderId, model: 'mock:mcp' },
  })
  expect(res.ok(), `create session failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function openMemory(page: Page) {
  await expect(page.locator('.chat-toolbar')).toBeVisible({ timeout: 10_000 })
  await page.getByTestId('chat-toolbar-menu').click()
  await page.getByTestId('chat-menu-memory').click()
  const modal = page.getByTestId('session-memory-modal')
  await expect(modal).toBeVisible()
  return modal
}

test('a short-term note shows how long it has left', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const folder = await makeFolder(request, auth)
  const session = await makeSession(request, auth, folder, 'short-term note')
  const note = 'Halfway through the login refactor'
  const sendRes = await request.post(`/api/sessions/${session}/message`, {
    headers: auth,
    data: { text: `note\n${mcpBlock('memory_add', { content: note })}`, model: 'mock:mcp' },
  })
  expect(sendRes.ok(), `send message failed: ${await sendRes.text()}`).toBeTruthy()
  await expect
    .poll(async () => (await listMemories(request, auth, session)).length, { timeout: 15_000 })
    .toBe(1)

  await loadAt(page, token, `/sessions/${session}`)
  const modal = await openMemory(page)
  const row = modal.getByTestId('session-memory-row')
  await expect(row).toContainText(note)
  // A fresh note: a whole lifetime ahead, not fading, not in the kept group.
  await expect(row.getByTestId('session-memory-expiry')).toHaveText(/^expires in \d+(h|d)$/)
  await expect(row.getByTestId('session-memory-fading')).toHaveCount(0)
  await expect(modal.getByTestId('session-memory-kept-heading')).toHaveCount(0)
  await shoot(page, 'session-tab', modal)
  await page.setViewportSize({ width: 380, height: 800 })
  await shoot(page, 'session-tab-narrow', modal)
})

test('a long-term note is shared by every session in the folder and can be edited and deleted', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const folder = await makeFolder(request, auth)
  const first = await makeSession(request, auth, folder, 'long-term writer')
  const second = await makeSession(request, auth, folder, 'long-term reader')

  // 1. Add a note from the first session's dialog.
  await loadAt(page, token, `/sessions/${first}`)
  let modal = await openMemory(page)
  await modal.getByTestId('memory-tab-folder').click()
  await expect(modal.getByTestId('folder-memory-empty')).toBeVisible()
  await modal.getByTestId('folder-memory-add-empty').click()
  // Saving with nothing filled in names the missing fields inline.
  await modal.getByTestId('folder-memory-save').click()
  await expect(modal.getByTestId('folder-memory-title-error')).toBeVisible()
  await expect(modal.getByTestId('folder-memory-content-error')).toBeVisible()
  await modal.getByTestId('folder-memory-category').selectOption('decision')
  await modal.getByTestId('folder-memory-title').fill('SQLite over Postgres')
  await modal.getByTestId('folder-memory-content').fill('One embedded binary, no server to run.')
  await modal.getByTestId('folder-memory-save').click()
  await expect(modal.getByTestId('folder-memory-editor')).toHaveCount(0)
  const decisionGroup = modal.getByTestId('folder-memory-group-decision')
  await expect(decisionGroup).toContainText('Design decisions')
  await expect(decisionGroup.getByTestId('folder-memory-row')).toContainText('SQLite over Postgres')
  await page.keyboard.press('Escape')
  await expect(modal).toHaveCount(0)

  // 2. The second session in the same folder sees it.
  await page.goto(`/sessions/${second}`)
  modal = await openMemory(page)
  await modal.getByTestId('memory-tab-folder').click()
  const row = modal.getByTestId('folder-memory-row')
  await expect(row).toHaveCount(1)
  await expect(row).toContainText('SQLite over Postgres')
  await expect(row).toContainText('One embedded binary, no server to run.')
  await shoot(page, 'folder-tab', modal)

  // 3. Edit: new title and category; the note moves to its new group.
  await row.getByTestId('folder-memory-edit').click()
  await expect(modal.getByTestId('folder-memory-category')).toHaveValue('decision')
  await shoot(page, 'folder-tab-editing', modal)
  await modal.getByTestId('folder-memory-category').selectOption('architecture')
  await modal.getByTestId('folder-memory-title').fill('Single embedded SQLite database')
  await modal.getByTestId('folder-memory-save').click()
  await expect(modal.getByTestId('folder-memory-group-decision')).toHaveCount(0)
  const archGroup = modal.getByTestId('folder-memory-group-architecture')
  await expect(archGroup).toContainText('Architecture')
  await expect(archGroup.getByTestId('folder-memory-row')).toContainText(
    'Single embedded SQLite database',
  )
  const listed = await request.get(`/api/folders/${folder}/memories`, { headers: auth })
  const body = (await listed.json()) as { memories: { title: string; category: string }[] }
  expect(body.memories.map((m) => [m.category, m.title])).toEqual([
    ['architecture', 'Single embedded SQLite database'],
  ])

  // 4. Delete, after confirming.
  await modal.getByTestId('folder-memory-delete').click()
  const confirm = page.getByTestId('confirm-delete-folder-memory')
  await expect(confirm).toBeVisible()
  await confirm.getByTestId('confirm-dialog-confirm').click()
  await expect(confirm).toHaveCount(0)
  await expect(modal.getByTestId('folder-memory-row')).toHaveCount(0)
  await expect(modal.getByTestId('folder-memory-empty')).toBeVisible()
  const after = await request.get(`/api/folders/${folder}/memories`, { headers: auth })
  expect(((await after.json()) as { memories: unknown[] }).memories).toEqual([])
})

test('the short-term memory lifetime setting saves and rejects an out-of-range value', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const before = await request.get('/api/settings/memory', { headers: auth })
  expect(before.ok(), `get memory settings failed: ${await before.text()}`).toBeTruthy()
  const original = (await before.json()) as {
    short_term_hours: number
    min_hours: number
    max_hours: number
  }

  await loadAt(page, token, '/settings/chat')
  const section = page.getByTestId('memory-settings-section')
  await expect(section).toBeVisible({ timeout: 15_000 })
  const input = section.getByTestId('memory-short-term-hours')
  await expect(input).toHaveValue(String(original.short_term_hours))

  // Out of range: inline error, Save disabled, nothing saved.
  await input.fill(String(original.max_hours + 1))
  await expect(section.getByTestId('memory-short-term-hours-error')).toContainText(
    `from ${original.min_hours} to ${original.max_hours}`,
  )
  await expect(section.getByTestId('memory-settings-save')).toBeDisabled()
  await shoot(page, 'setting-error', section)

  // In range: saves.
  const next = original.short_term_hours === 12 ? 13 : 12
  await input.fill(String(next))
  await expect(section.getByTestId('memory-short-term-hours-error')).toHaveCount(0)
  await section.getByTestId('memory-settings-save').click()
  await expect(section).toContainText('Saved.')
  const saved = await request.get('/api/settings/memory', { headers: auth })
  expect(((await saved.json()) as { short_term_hours: number }).short_term_hours).toBe(next)
  await shoot(page, 'setting', section)

  // Restore for later specs.
  const restore = await request.put('/api/settings/memory', {
    headers: auth,
    data: { short_term_hours: original.short_term_hours },
  })
  expect(restore.status()).toBe(204)
})
