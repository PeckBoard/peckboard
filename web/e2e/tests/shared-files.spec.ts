import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Files an agent shares into the chat (`share_file` MCP tool →
 * `file-shared` event → SharedFileCard):
 *
 *  - a text file, a PNG and a .zip each render a card with name + size and
 *    a Download button; the PNG card shows an inline thumbnail;
 *  - clicking a card opens the preview modal: highlighted text for the
 *    markdown file, the full image for the PNG, download-only for the zip;
 *  - Download hands the browser a signed URL that answers 200 with
 *    `Content-Disposition: attachment`; Range requests answer 206; a
 *    tampered signature is refused;
 *  - a path outside the session folder is rejected by the tool.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

// 1×1 transparent PNG.
const PNG_1X1 = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==',
  'base64',
)

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

/** A session in a fresh folder holding the files to share, with a live
 *  `mock:block` agent so the per-session MCP token stays valid. */
async function seedSession(request: APIRequestContext, authHeader: Record<string, string>) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-shared-'))
  writeFileSync(path.join(folderPath, 'notes.md'), '# Release notes\n\nshared-text-marker\n')
  writeFileSync(path.join(folderPath, 'pixel.png'), PNG_1X1)
  writeFileSync(
    path.join(folderPath, 'bundle.zip'),
    Buffer.from('PK\u0005\u0006' + '\0'.repeat(18)),
  )
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-shared-files', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'shared files', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
    headers: authHeader,
    data: { text: 'go', model: 'mock:block' },
  })
  expect(sendRes.ok(), `send message failed: ${await sendRes.text()}`).toBeTruthy()
  const cfgPath = path.join(process.env.PECKBOARD_E2E_DATA_DIR!, 'worker-mcp', `${session.id}.json`)
  await expect
    .poll(() => {
      try {
        readFileSync(cfgPath, 'utf8')
        return true
      } catch {
        return false
      }
    })
    .toBe(true)
  const cfg = JSON.parse(readFileSync(cfgPath, 'utf8')) as {
    mcpServers: { peckboard: { headers: { Authorization: string } } }
  }
  const mcpToken = cfg.mcpServers.peckboard.headers.Authorization.replace(/^Bearer /, '')
  return { sessionId: session.id, mcpToken }
}

async function mcpCall(
  request: APIRequestContext,
  mcpToken: string,
  tool: string,
  args: Record<string, unknown>,
): Promise<{
  result?: { content: { text?: string }[]; isError?: boolean }
  error?: { message: string }
}> {
  const res = await request.post('/mcp', {
    headers: { Authorization: `Bearer ${mcpToken}` },
    data: { jsonrpc: '2.0', id: 1, method: 'tools/call', params: { name: tool, arguments: args } },
  })
  expect(res.ok()).toBeTruthy()
  return res.json()
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

test('shared files render as cards with preview and download', async ({ request, page }) => {
  const { token, authHeader } = await authenticate(request)
  const { sessionId, mcpToken } = await seedSession(request, authHeader)

  // Jail: a path outside the folder is refused, nothing is shared.
  const bad = await mcpCall(request, mcpToken, 'share_file', { path: '../../etc/passwd' })
  const badText = JSON.stringify(bad)
  expect(badText).toMatch(/project folder/)

  for (const [p, caption] of [
    ['notes.md', 'Release notes'],
    ['pixel.png', undefined],
    ['bundle.zip', 'The build'],
  ] as const) {
    const body = await mcpCall(
      request,
      mcpToken,
      'share_file',
      caption ? { path: p, caption } : { path: p },
    )
    expect(body.error, `share_file ${p} failed: ${JSON.stringify(body)}`).toBeFalsy()
    expect(body.result?.isError).toBeFalsy()
  }

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  const cards = page.getByTestId('shared-file-card')
  await expect(cards).toHaveCount(3, { timeout: 10_000 })

  const textCard = cards.filter({ hasText: 'notes.md' })
  const pngCard = cards.filter({ hasText: 'pixel.png' })
  const zipCard = cards.filter({ hasText: 'bundle.zip' })
  await expect(textCard).toContainText('Release notes')
  await expect(zipCard).toContainText('The build')
  for (const c of [textCard, pngCard, zipCard]) {
    await expect(c.getByTestId('shared-file-download')).toBeVisible()
  }

  // Image thumbnail actually loads through the signed URL.
  const thumb = pngCard.getByTestId('shared-file-thumb')
  await expect(thumb).toBeVisible()
  await expect
    .poll(() => thumb.evaluate((img) => (img as HTMLImageElement).naturalWidth))
    .toBeGreaterThan(0)

  // Text preview.
  await textCard.getByTestId('shared-file-open').click()
  const modal = page.getByTestId('shared-file-modal')
  await expect(modal.getByTestId('shared-file-preview-text')).toContainText('shared-text-marker')
  await expect(modal.getByTestId('shared-file-download')).toBeVisible()
  await modal.getByRole('button', { name: 'Close preview' }).click()
  await expect(modal).toBeHidden()

  // Image preview.
  await pngCard.getByTestId('shared-file-open').click()
  await expect(modal.getByTestId('shared-file-preview-img')).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(modal).toBeHidden()

  // Zip: no preview, download only.
  await zipCard.getByTestId('shared-file-open').click()
  await expect(modal.getByTestId('shared-file-nopreview')).toBeVisible()
  const [download] = await Promise.all([
    page.waitForEvent('download'),
    modal.getByTestId('shared-file-download').click(),
  ])
  expect(download.suggestedFilename()).toBe('bundle.zip')
  await page.keyboard.press('Escape')

  // Signed-URL route: attachment disposition, Range support, tamper refusal.
  const eventsRes = await request.get(`/api/sessions/${sessionId}/events?after_seq=0`, {
    headers: authHeader,
  })
  const events = (await eventsRes.json()) as {
    kind: string
    data: { id: string; filename: string }
  }[]
  const shared = events.filter((e) => e.kind === 'file-shared')
  expect(shared.map((e) => e.data.filename).sort()).toEqual(['bundle.zip', 'notes.md', 'pixel.png'])
  const zipId = shared.find((e) => e.data.filename === 'bundle.zip')!.data.id
  const mint = await request.post(`/api/sessions/${sessionId}/shared-files/${zipId}/url`, {
    headers: authHeader,
  })
  expect(mint.ok()).toBeTruthy()
  const { url } = (await mint.json()) as { url: string }

  const dl = await request.get(`${url}&download=1`)
  expect(dl.status()).toBe(200)
  expect(dl.headers()['content-disposition']).toMatch(/^attachment;.*bundle\.zip/)
  expect(dl.headers()['x-content-type-options']).toBe('nosniff')

  const ranged = await request.get(url, { headers: { Range: 'bytes=0-1' } })
  expect(ranged.status()).toBe(206)
  expect(ranged.headers()['content-range']).toBe('bytes 0-1/22')
  expect((await ranged.body()).toString('latin1')).toBe('PK')

  const tampered = url.replace(/sig=([0-9a-f])/, (_, c: string) => `sig=${c === '0' ? '1' : '0'}`)
  expect((await request.get(tampered)).status()).toBe(403)
  // Minting requires auth.
  const anon = await request.post(`/api/sessions/${sessionId}/shared-files/${zipId}/url`)
  expect(anon.status()).toBe(401)
})
