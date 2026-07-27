import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Tool-card + crash-row + payload rendering (render-polish / taxonomy /
 * payload-budget cards):
 *
 *  - Long tool stdout clamps to 40 lines with "Show all N lines" /
 *    "Collapse" (aria-expanded) and a Copy button.
 *  - A native Edit card renders the old/new mini-diff; a Write card
 *    renders the syntax-highlighted content preview.
 *  - `mock:crash` renders the crash row with reason + exit code and an
 *    expandable stderr pane.
 *  - The server-side truncation marker string renders verbatim in the
 *    output pane; a legacy inline-base64 tool image still renders while
 *    new screenshots arrive as blob references (`images: [{id, ...}]`)
 *    and load via the tool-images route.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const PNG_1X1 =
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=='

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

async function seedSession(request: APIRequestContext, authHeader: Record<string, string>) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-render-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-render', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'render verify', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  return session.id
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

async function injectEvent(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  sessionId: string,
  kind: string,
  data: Record<string, unknown>,
) {
  const res = await request.post(`/api/sessions/${sessionId}/events`, {
    headers: authHeader,
    data: { kind, data },
  })
  expect(res.ok(), `inject ${kind} failed: ${await res.text()}`).toBeTruthy()
}

test('tool cards: stdout clamp with Show all/Copy, Edit mini-diff, Write preview', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)

  const lines = Array.from({ length: 60 }, (_, i) => `stdout line ${i + 1}`).join('\n')
  await injectEvent(request, authHeader, sessionId, 'agent-tool-start', {
    toolUseId: 'tu-bash-1',
    name: 'Bash',
    input: { command: 'seq 60', description: 'Print 60 lines' },
  })
  await injectEvent(request, authHeader, sessionId, 'agent-tool-end', {
    toolUseId: 'tu-bash-1',
    output: lines,
  })
  await injectEvent(request, authHeader, sessionId, 'agent-tool-start', {
    toolUseId: 'tu-edit-1',
    name: 'Edit',
    input: {
      file_path: 'src/alpha.ts',
      old_string: 'const answer = 41',
      new_string: 'const answer = 42',
    },
  })
  await injectEvent(request, authHeader, sessionId, 'agent-tool-end', {
    toolUseId: 'tu-edit-1',
    output: 'ok',
  })
  await injectEvent(request, authHeader, sessionId, 'agent-tool-start', {
    toolUseId: 'tu-write-1',
    name: 'Write',
    input: { file_path: 'src/new.ts', content: 'export const created = true\n' },
  })
  await injectEvent(request, authHeader, sessionId, 'agent-tool-end', {
    toolUseId: 'tu-write-1',
    output: 'created',
  })

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  // Bash card: clamp at 40 lines, expander announces state.
  await page.getByRole('button', { name: /seq 60/ }).click()
  await expect(page.getByText('… 20 more lines')).toBeVisible()
  const showAll = page.getByRole('button', { name: 'Show all 60 lines' })
  await expect(showAll).toHaveAttribute('aria-expanded', 'false')
  await expect(page.getByRole('button', { name: 'Copy' }).first()).toBeVisible()
  await showAll.click()
  await expect(page.getByText('stdout line 60')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Collapse' })).toHaveAttribute(
    'aria-expanded',
    'true',
  )

  // Edit card: mini-diff shows the old/new pair as del/add rows.
  await page.getByRole('button', { name: /Edit file src\/alpha\.ts/ }).click()
  const minidiff = page.locator('.tool-minidiff')
  await expect(minidiff).toBeVisible()
  await expect(minidiff.locator('.diff-line-del')).toContainText('const answer = 41')
  await expect(minidiff.locator('.diff-line-add')).toContainText('const answer = 42')

  // Write card: content preview pane.
  await page.getByRole('button', { name: /Write file src\/new\.ts/ }).click()
  await expect(page.locator('.tool-code-preview')).toContainText('export const created = true')
})

test('mock:crash renders crash row with exit code and expandable stderr', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-vrow').first())).toBeVisible({
    timeout: 10_000,
  })

  const send = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'crash please', model: 'mock:crash' },
  })
  expect(send.ok()).toBeTruthy()

  await expect(page.getByText('Agent crashed')).toBeVisible({ timeout: 10_000 })
  await expect(page.getByText('mock scenario crash (exit 1)')).toBeVisible()

  const details = page.getByTestId('chat-crash-details')
  await expect(details).toBeVisible()
  await details.locator('summary').click()
  await expect(details).toContainText('simulated stderr')
})

test('payload: truncation marker renders; legacy base64 image and blob-reference image both render', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)

  // Legacy inline-base64 image (pre-blob-offload event shape).
  await injectEvent(request, authHeader, sessionId, 'agent-tool-start', {
    toolUseId: 'tu-legacy-img',
    name: 'mcp__playwright__browser_take_screenshot',
    input: {},
  })
  await injectEvent(request, authHeader, sessionId, 'agent-tool-end', {
    toolUseId: 'tu-legacy-img',
    output: 'screenshot taken',
    images: [{ mimeType: 'image/png', dataBase64: PNG_1X1 }],
  })
  // Output that carries the server-side truncation marker.
  await injectEvent(request, authHeader, sessionId, 'agent-tool-start', {
    toolUseId: 'tu-trunc',
    name: 'Bash',
    input: { command: 'cat big.log' },
  })
  await injectEvent(request, authHeader, sessionId, 'agent-tool-end', {
    toolUseId: 'tu-trunc',
    output: 'head of output\n… [truncated: 262144 bytes total]',
  })

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  // Legacy image renders from the inline payload.
  const legacyImg = page.locator('img[alt="Screenshot"]')
  await expect(legacyImg).toBeVisible({ timeout: 10_000 })
  await expect(legacyImg).toHaveAttribute('src', /^data:image\/png;base64|^blob:/)

  // Marker text renders verbatim in the output pane.
  await page.getByRole('button', { name: /cat big\.log/ }).click()
  await expect(page.getByText('[truncated: 262144 bytes total]')).toBeVisible()

  // New-path screenshot: the mock scenario emits a blob reference, and
  // the wire event must carry an image id, not inline base64.
  const send = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'shot', model: 'mock:screenshot' },
  })
  expect(send.ok()).toBeTruthy()
  await expect(page.getByText('Done.')).toBeVisible({ timeout: 10_000 })

  const eventsRes = await request.get(`/api/sessions/${sessionId}/events?after_seq=0`, {
    headers: authHeader,
  })
  const all = (await eventsRes.json()) as {
    kind: string
    data: { images?: { id?: string; dataBase64?: string; mimeType: string }[] }
  }[]
  const blobEnd = all.find(
    (e) => e.kind === 'agent-tool-end' && e.data.images?.length && e.data.images[0].id,
  )
  expect(blobEnd, 'screenshot tool-end must carry a blob image id').toBeTruthy()
  expect(blobEnd!.data.images![0].dataBase64).toBeUndefined()

  // The blob id resolves through the authed tool-images route.
  const imgRes = await request.get(
    `/api/sessions/${sessionId}/tool-images/${blobEnd!.data.images![0].id}`,
    { headers: authHeader },
  )
  expect(imgRes.ok(), `tool-image fetch failed: ${imgRes.status()}`).toBeTruthy()
  expect(imgRes.headers()['content-type']).toContain('image/png')
})
