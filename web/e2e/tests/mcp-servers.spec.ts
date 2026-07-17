import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import fs from 'node:fs/promises'
import os from 'node:os'
import path from 'node:path'

/**
 * UI e2e for Settings → MCP Servers (the user-defined MCP server editor).
 *
 * The section renders from `GET /api/settings/mcp-servers` and persists
 * whole-list edits via PUT. Injection into per-session config files is
 * covered by Rust unit tests (`service::mcp_server::user_servers`,
 * `provider::cursor::mcp`); here we verify the editor itself:
 *
 * 1. Empty state → add a stdio server through the modal → card appears.
 * 2. The list survives a reload (persisted server-side).
 * 3. Import JSON adds a server from a pasted `mcpServers` snippet.
 * 4. Validation: the reserved name `peckboard` blocks saving.
 * 5. Enable toggle + delete round-trip.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

async function openMcpSettings(page: Page, token: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/settings')
  await page.getByTestId('settings-nav-mcp').click()
  await expect(page.getByTestId('mcp-servers-section')).toBeVisible({ timeout: 10_000 })
}

test('MCP server editor: add, import, validate, toggle, delete', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)

  // Start from a clean slate so the spec is re-runnable against a reused
  // dev server (`reuseExistingServer` keeps state across local runs).
  const wipe = await request.put('/api/settings/mcp-servers', {
    data: { servers: [] },
    headers: { Authorization: `Bearer ${token}` },
  })
  expect(wipe.ok()).toBeTruthy()

  await openMcpSettings(page, token)
  await expect(page.getByTestId('mcp-empty')).toBeVisible()

  // ── Add a stdio server through the modal ─────────────────────────
  await page.getByTestId('mcp-add-server').click()
  const modal = page.getByTestId('mcp-server-modal')
  await expect(modal).toBeVisible()
  await page.getByTestId('mcp-field-name').fill('github')
  await page.getByTestId('mcp-field-command').fill('npx')
  await expect(page.getByTestId('mcp-server-save')).toBeEnabled()
  await page.getByTestId('mcp-server-save').click()
  await expect(modal).toHaveCount(0)

  const card = page.getByTestId('mcp-server-card-github')
  await expect(card).toBeVisible()
  await expect(card).toContainText('github')
  await expect(card).toContainText('stdio')
  await expect(card).toContainText('npx')
  await expect(page.getByTestId('mcp-empty')).toHaveCount(0)

  // ── Persisted server-side: survives a reload ─────────────────────
  await page.reload()
  await page.getByTestId('settings-nav-mcp').click()
  await expect(page.getByTestId('mcp-server-card-github')).toBeVisible({ timeout: 10_000 })

  // And the API reflects it.
  const listed = await request.get('/api/settings/mcp-servers', {
    headers: { Authorization: `Bearer ${token}` },
  })
  expect(listed.ok()).toBeTruthy()
  const body = (await listed.json()) as {
    servers: { name: string; transport: string }[]
    supported_providers: string[]
  }
  expect(body.servers.map((s) => s.name)).toEqual(['github'])
  expect(body.supported_providers).toContain('claude')
  expect(body.supported_providers).toContain('cursor')
  expect(body.supported_providers).toContain('grok')
  expect(body.supported_providers).toContain('ollama')

  // ── Import JSON (the snippet shape MCP READMEs ship) ─────────────
  await page.getByTestId('mcp-import-json').click()
  await page
    .getByTestId('mcp-import-textarea')
    .fill('{"mcpServers": {"linear": {"type": "http", "url": "https://linear.app/mcp"}}}')
  await expect(page.getByTestId('mcp-import-confirm')).toBeEnabled()
  await page.getByTestId('mcp-import-confirm').click()
  const linearCard = page.getByTestId('mcp-server-card-linear')
  await expect(linearCard).toBeVisible()
  await expect(linearCard).toContainText('HTTP')
  await expect(linearCard).toContainText('https://linear.app/mcp')

  // Visual proof for review: the populated list, then the edit modal.
  await page.screenshot({ path: 'e2e/test-results/mcp-servers-list.png', fullPage: true })
  await githubEditShot(page)

  // ── Validation: the reserved built-in name cannot be used ────────
  await page.getByTestId('mcp-add-server').click()
  await page.getByTestId('mcp-field-name').fill('peckboard')
  await page.getByTestId('mcp-field-command').fill('echo')
  await expect(page.getByTestId('mcp-server-modal')).toContainText('reserved')
  await expect(page.getByTestId('mcp-server-save')).toBeDisabled()
  await page.keyboard.press('Escape')
  await expect(page.getByTestId('mcp-server-modal')).toHaveCount(0)

  // ── Enable toggle round-trips ────────────────────────────────────
  const githubCard = page.getByTestId('mcp-server-card-github')
  // The real checkbox is opacity-0 over the styled slider, so click the
  // switch itself rather than setChecked (which requires visibility).
  await githubCard.locator('.mcp-switch').click()
  await expect(githubCard).toHaveClass(/mcp-server-card--off/)

  // ── Delete with confirm ──────────────────────────────────────────
  await linearCard.getByRole('button', { name: 'Delete' }).click()
  await page.getByTestId('mcp-delete-confirm').click()
  await expect(page.getByTestId('mcp-server-card-linear')).toHaveCount(0)
  await expect(page.getByTestId('mcp-server-card-github')).toBeVisible()
})

/** Open the github card's edit modal, screenshot it, close it. */
async function githubEditShot(page: Page) {
  await page.getByTestId('mcp-server-card-github').getByRole('button', { name: 'Edit' }).click()
  await expect(page.getByTestId('mcp-server-modal')).toBeVisible()
  // Viewport shot (fullPage stitching washes out the fixed-position modal),
  // after the backdrop fade-in settles so the capture isn't mid-animation.
  await page.waitForTimeout(400)
  await page.screenshot({ path: 'e2e/test-results/mcp-server-edit-modal.png' })
  await page.keyboard.press('Escape')
  await expect(page.getByTestId('mcp-server-modal')).toHaveCount(0)
}

/**
 * Live tools panel: a real stdio MCP server (a tiny node script written to a
 * temp dir) is probed from the card's Tools panel; per-tool switches persist
 * as `disabled_tools`; the editor's Test connection reports the tool count;
 * a broken command surfaces the probe error.
 */
test('tools panel: probe, per-tool toggle persistence, test connection, error', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)

  // A minimal MCP stdio server: answers initialize + tools/list.
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'pb-mcp-e2e-'))
  const serverPath = path.join(dir, 'fake-mcp.cjs')
  await fs.writeFile(
    serverPath,
    `
const readline = require('node:readline')
const rl = readline.createInterface({ input: process.stdin })
rl.on('line', (line) => {
  let msg
  try { msg = JSON.parse(line) } catch { return }
  if (msg.id === undefined) return
  let result = {}
  if (msg.method === 'initialize') {
    result = { protocolVersion: '2025-06-18', capabilities: { tools: {} }, serverInfo: { name: 'fake', version: '0.0.1' } }
  } else if (msg.method === 'tools/list') {
    result = { tools: [
      { name: 'alpha', description: 'first tool', inputSchema: { type: 'object' } },
      { name: 'beta', description: 'second tool', inputSchema: { type: 'object' } },
    ] }
  }
  process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: msg.id, result }) + '\\n')
})
`,
  )

  // Seed one working and one broken server via the API.
  const put = await request.put('/api/settings/mcp-servers', {
    data: {
      servers: [
        {
          id: 'e2e-fake',
          name: 'fake',
          transport: 'stdio',
          command: 'node',
          args: [serverPath],
          env: [],
          url: '',
          headers: [],
          enabled: true,
          providers: [],
          disabled_tools: [],
        },
        {
          id: 'e2e-broken',
          name: 'broken',
          transport: 'stdio',
          command: 'definitely-not-a-real-command-xyz',
          args: [],
          env: [],
          url: '',
          headers: [],
          enabled: true,
          providers: [],
          disabled_tools: [],
        },
      ],
    },
    headers: { Authorization: `Bearer ${token}` },
  })
  expect(put.ok(), `seed failed: ${await put.text()}`).toBeTruthy()

  await openMcpSettings(page, token)

  // ── Probe lists the fake server's tools ──────────────────────
  await page.getByTestId('mcp-tools-toggle-fake').click()
  const alpha = page.getByTestId('mcp-tool-toggle-fake-alpha')
  await expect(alpha).toBeVisible({ timeout: 15_000 })
  await expect(alpha).toBeChecked()
  await expect(page.getByTestId('mcp-tool-toggle-fake-beta')).toBeChecked()

  // ── Switching a tool off persists as disabled_tools ──────────────
  await alpha.click()
  await expect(alpha).not.toBeChecked()
  await expect
    .poll(async () => {
      const res = await request.get('/api/settings/mcp-servers', {
        headers: { Authorization: `Bearer ${token}` },
      })
      const body = (await res.json()) as {
        servers: { name: string; disabled_tools?: string[] }[]
      }
      return body.servers.find((s) => s.name === 'fake')?.disabled_tools ?? []
    })
    .toEqual(['alpha'])
  await expect(page.getByTestId('mcp-tools-toggle-fake')).toContainText('1 off')

  await page.screenshot({ path: 'e2e/test-results/mcp-tools-panel.png', fullPage: true })

  // ── Test connection from the editor reports the tool count ───────
  await page.getByTestId('mcp-server-card-fake').getByRole('button', { name: 'Edit' }).click()
  await page.getByTestId('mcp-test-connection').click()
  await expect(page.getByTestId('mcp-test-result')).toContainText('2 tools', { timeout: 15_000 })
  await page.keyboard.press('Escape')
  await expect(page.getByTestId('mcp-server-modal')).toHaveCount(0)

  // ── A dead command surfaces the probe error ────────────────────
  await page.getByTestId('mcp-tools-toggle-broken').click()
  await expect(page.getByTestId('mcp-tools-error-broken')).toBeVisible({ timeout: 15_000 })
})
