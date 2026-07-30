import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e test: tool rows read the same whichever provider produced them.
 *
 * Claude's vocabulary (`Read`, `Bash`, …) used to be the only one the chat
 * knew, so a cursor/grok/kimi turn rendered raw internal names — a column of
 * `mcp` and `getMcpTools` rows with no summary. Driven by `mock:cli-tools`,
 * which emits exactly what the cursor parser now produces: cursor's own
 * `shell`/`read`/`edit` names, an MCP call unwrapped to
 * `mcp__<server>__<tool>`, the `getMcpTools` listing call, and the diff an
 * edit hands back.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

async function seedSession(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  name: string,
): Promise<string> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-rows-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name, folder_id: folder.id },
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

test('non-Claude tool names render as labelled, summarised rows', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader, 'e2e-provider-rows')

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-bubble').first())).toBeVisible({
    timeout: 10_000,
  })

  const sendRes = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'go', model: 'mock:cli-tools' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  const rows = page.locator('.tool-block')
  await expect(rows).toHaveCount(5, { timeout: 15_000 })

  // 1. Cursor's `shell` is an exec tool: command line + why-sentence, no
  //    tool name anywhere.
  const shell = rows.nth(0)
  await expect(shell.locator('.tool-reason-primary')).toHaveText('Build the release binary')
  await expect(shell.locator('.tool-cmd')).toHaveText('cargo build --release')
  await expect(shell).not.toContainText('shell')

  // 2. An MCP call the parser unwrapped — labelled and summarised like the
  //    same call from Claude, instead of the bare word `mcp`.
  const mcp = rows.nth(1)
  await expect(mcp.locator('.tool-label')).toHaveText('Search content')
  await expect(mcp.locator('.tool-summary')).toContainText('needle')
  await expect(mcp).not.toContainText('mcp__')

  // 3. The MCP tool-listing call, named for what it does.
  const listing = rows.nth(2)
  await expect(listing.locator('.tool-label')).toHaveText('List MCP tools')
  await expect(listing.locator('.tool-summary')).toHaveText('peckboard')
  await expect(listing).not.toContainText('getMcpTools')

  // 4. Cursor's lowercase `read` lands on the same label as Claude's `Read`.
  const read = rows.nth(3)
  await expect(read.locator('.tool-label')).toHaveText('Read file')
  await expect(read.locator('.tool-summary')).toContainText('lib.rs')

  // 5. An edit carries its diff card, the same one Peckboard's own
  //    edit_file produces.
  const edit = rows.nth(4)
  await expect(edit.locator('.tool-label')).toHaveText('Edit file')
  const diff = edit.getByTestId('diff-block')
  await expect(diff).toBeVisible()
  await expect(diff.locator('.diff-path')).toHaveText('/workspace/src/lib.rs')
  await diff.locator('.diff-header').click()
  // The body carries the hunk; the `+++` header line is an add-coloured
  // line too, so assert on the body rather than a single line.
  await expect(diff.locator('.diff-body')).toContainText('+new')

  await page.screenshot({ path: 'test-results/provider-tool-rows.png', fullPage: true })
})
