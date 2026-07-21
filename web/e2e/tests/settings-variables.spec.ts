import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import os from 'node:os'
import path from 'node:path'

/**
 * UI e2e for the two variable surfaces in Settings:
 *
 * 1. Settings → Agent Variables (`agent-vars-section`) — plain key/value
 *    state agents read AND write via MCP tools; users manage it here.
 *    Covers: add a global var, add a folder-scoped var through the scope
 *    <select>, folder badge rendering, edit-in-place, delete.
 * 2. Settings → Environment Variables (`env-vars-section`) — now scoped
 *    too. Covers: the scope select exists, a folder-scoped var carries the
 *    folder badge, and delete works against the id-based DELETE route.
 *
 * Both flows wipe their lists up front so the spec is re-runnable against
 * a reused dev server (`reuseExistingServer` keeps state across runs).
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

/** Ensure a folder exists to scope variables to; returns { id, name }. */
async function ensureFolder(
  request: APIRequestContext,
  auth: Record<string, string>,
): Promise<{ id: string; name: string }> {
  const name = `vars-e2e-${Date.now()}`
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name, path: path.join(os.tmpdir(), name), create: true },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  const folder = (await res.json()) as { id: string; name: string }
  return { id: folder.id, name: folder.name }
}

/** Delete every row the list endpoint returns (id-based DELETE). */
async function wipeVars(
  request: APIRequestContext,
  auth: Record<string, string>,
  endpoint: '/api/agent-vars' | '/api/env-vars',
) {
  const res = await request.get(endpoint, { headers: auth })
  expect(res.ok()).toBeTruthy()
  const { vars } = (await res.json()) as { vars: Array<{ id: string }> }
  for (const v of vars) {
    const del = await request.delete(`${endpoint}/${encodeURIComponent(v.id)}`, {
      headers: auth,
    })
    expect(del.ok(), `wipe ${endpoint}/${v.id} failed`).toBeTruthy()
  }
}

async function openSettings(page: Page, token: string, navTestId: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/settings')
  await page.getByTestId(navTestId).click()
}

test('Agent Variables: global + folder-scoped add, shadow badge, edit, delete', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)
  const auth = { Authorization: `Bearer ${token}` }
  const folder = await ensureFolder(request, auth)
  await wipeVars(request, auth, '/api/agent-vars')

  await openSettings(page, token, 'settings-nav-variables')
  const section = page.getByTestId('agent-vars-section')
  await expect(section).toBeVisible({ timeout: 10_000 })

  // ── Add a global var ──────────────────────────────────────────────
  await page.getByTestId('agent-var-name-input').fill('GREETING')
  await page.getByTestId('agent-var-value-input').fill('hello')
  await page.getByTestId('agent-var-save-btn').click()
  const globalRow = page.getByTestId('agent-var-GREETING')
  await expect(globalRow).toBeVisible()
  await expect(globalRow).toContainText('Global')
  await expect(globalRow).toContainText('hello')

  // ── Add a folder-scoped var via the scope select ──────────────────
  await page.getByTestId('agent-var-name-input').fill('TARGET')
  await page.getByTestId('agent-var-value-input').fill('prod')
  await page.getByTestId('agent-var-scope-select').selectOption(folder.id)
  await page.getByTestId('agent-var-save-btn').click()
  const scopedRow = page.getByTestId('agent-var-TARGET')
  await expect(scopedRow).toBeVisible()
  await expect(scopedRow).toContainText(folder.name)
  await expect(scopedRow).toContainText('prod')

  // ── Edit the global var in place ──────────────────────────────────
  await page.getByTestId('agent-var-edit-GREETING').click()
  await page.getByTestId('agent-var-value-input').fill('bonjour')
  await page.getByTestId('agent-var-save-btn').click()
  await expect(page.getByTestId('agent-var-GREETING')).toContainText('bonjour')

  // ── Delete both ───────────────────────────────────────────────────
  await page.getByTestId('agent-var-delete-GREETING').click()
  await expect(page.getByTestId('agent-var-GREETING')).toHaveCount(0)
  await page.getByTestId('agent-var-delete-TARGET').click()
  await expect(page.getByTestId('agent-var-TARGET')).toHaveCount(0)
})

test('Environment Variables: scope select adds a folder-scoped var; id delete', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)
  const auth = { Authorization: `Bearer ${token}` }
  const folder = await ensureFolder(request, auth)
  await wipeVars(request, auth, '/api/env-vars')

  await openSettings(page, token, 'settings-nav-env')
  const section = page.getByTestId('env-vars-section')
  await expect(section).toBeVisible({ timeout: 10_000 })

  // Folder-scoped plaintext var through the form.
  await page.getByTestId('env-var-name-input').fill('API_HOST')
  await page.getByTestId('env-var-value-input').fill('folder.example')
  await page.getByTestId('env-var-scope-select').selectOption(folder.id)
  await page.getByTestId('env-var-save-btn').click()
  const row = page.getByTestId('env-var-API_HOST')
  await expect(row).toBeVisible()
  await expect(row).toContainText(folder.name)

  // Same name can coexist globally — the (name, scope) upsert keeps them
  // as two rows.
  await page.getByTestId('env-var-name-input').fill('API_HOST')
  await page.getByTestId('env-var-value-input').fill('global.example')
  await page.getByTestId('env-var-scope-select').selectOption('')
  await page.getByTestId('env-var-save-btn').click()
  await expect(page.getByTestId('env-var-API_HOST')).toHaveCount(2)

  // Delete both rows (id-based route) — the list drains.
  for (let i = 0; i < 2; i++) {
    await page.getByTestId('env-var-delete-API_HOST').first().click()
    await expect(page.getByTestId('env-var-API_HOST')).toHaveCount(1 - i)
  }
})
