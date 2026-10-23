import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e for Settings → External Access (OAuth client_credentials clients).
 *
 * Drives the user-visible loop end to end: create a client scoped to a
 * folder, read its one-time secret out of the reveal dialog, trade it for an
 * access token at the token URL, then revoke it through the confirmation and
 * prove the same credentials are now refused. No agent run is involved.
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

async function loadApp(page: Page, token: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/')
  await expect(page.locator('.rail-brand')).toBeVisible({ timeout: 10_000 })
}

async function openExternalAccess(page: Page) {
  await page.locator('.rail-avatar').click()
  const menu = page.locator('.user-menu-dropdown')
  await expect(menu).toBeVisible()
  await menu.getByRole('menuitem', { name: 'Settings' }).click()
  const settings = page.getByTestId('settings-page')
  await expect(settings).toBeVisible()
  await settings.getByTestId('settings-nav-external-access').click()
  const section = settings.getByTestId('external-access-section')
  await expect(section).toBeVisible()
  return section
}

async function requestToken(
  request: APIRequestContext,
  tokenUrl: string,
  clientId: string,
  secret: string,
) {
  // The token URL is absolute (it may name the HTTPS port); post to its path
  // on the suite's server so the request rides the harness base URL.
  const { pathname } = new URL(tokenUrl)
  return request.post(pathname, {
    headers: {
      Authorization: `Basic ${Buffer.from(`${clientId}:${secret}`).toString('base64')}`,
    },
    form: { grant_type: 'client_credentials' },
  })
}

test('create an OAuth client, use its secret, then revoke it', async ({ request, page }) => {
  const auth = await authenticate(request)
  const folderName = `ext-access-${Date.now()}`
  const folderId = await createFolder(request, auth, folderName)
  const clientName = `e2e-ext-${Date.now()}`

  await loadApp(page, auth.token)
  const section = await openExternalAccess(page)
  await expect(section.getByTestId('ext-endpoint-token-url')).toHaveValue(/\/oauth|token/)
  await expect(section.getByTestId('ext-endpoint-mcp-url')).toHaveValue(/mcp/)

  // ── Create ─────────────────────────────────────────────────────────
  await section.getByTestId('ext-client-new').click()
  const modal = page.getByTestId('ext-client-create-modal')
  await expect(modal).toBeVisible()
  // The primary action states why it's disabled until the client is named.
  await expect(modal.getByTestId('ext-client-disabled-reason')).toBeVisible()
  await expect(modal.getByTestId('ext-client-submit')).toBeDisabled()

  await modal.getByTestId('ext-client-name').fill(clientName)
  await modal.getByTestId('ext-client-folder').selectOption(folderId)
  await expect(modal.getByTestId('ext-client-project')).toHaveValue('')
  await modal.getByTestId('ext-client-submit').click()
  await expect(modal).toBeHidden()

  // ── One-time secret ────────────────────────────────────────────────
  const reveal = page.getByTestId('ext-client-secret-modal')
  await expect(reveal).toBeVisible()
  await expect(reveal.getByTestId('ext-client-secret-warning')).toContainText('only this once')
  const secret = await reveal.getByTestId('ext-client-secret-value').inputValue()
  expect(secret).toMatch(/^pbcs_/)
  const clientId = await reveal.getByTestId('ext-client-secret-client-id').inputValue()
  expect(clientId).not.toBe('')
  const tokenUrl = await reveal.getByTestId('ext-client-secret-token-url').inputValue()
  await expect(reveal.getByTestId('ext-client-secret-curl')).toHaveValue(
    new RegExp(`-u '${clientId}:${secret}' -d grant_type=client_credentials`),
  )
  await expect(reveal.getByTestId('ext-client-secret-mcp-config')).toHaveValue(/"Authorization"/)
  await reveal.getByTestId('ext-client-secret-done').click()
  await expect(reveal).toBeHidden()

  // ── The row carries the name, scope and client id ──────────────────
  await expect(section.getByTestId(`ext-client-row-${clientName}`)).toBeVisible()
  await expect(section.getByTestId(`ext-client-scope-${clientName}`)).toHaveText(folderName)
  await expect(section.getByTestId(`ext-client-id-${clientName}`)).toHaveText(clientId)

  // ── The credentials work at the token endpoint ─────────────────────
  const ok = await requestToken(request, tokenUrl, clientId, secret)
  expect(ok.status(), await ok.text()).toBe(200)
  const body = (await ok.json()) as { access_token?: string }
  expect(body.access_token).toBeTruthy()

  // ── Revoke, via the row's 3-dot menu and the shared confirm ────────
  const row = section.locator('.list-view-row').filter({ hasText: clientName })
  await row.locator('.list-view-menu').click()
  await page.getByRole('menuitem', { name: 'Revoke' }).click()
  const confirm = page.getByTestId('ext-client-revoke-confirm')
  await expect(confirm).toBeVisible()
  await confirm.getByTestId('confirm-dialog-confirm').click()
  await expect(confirm).toBeHidden()
  await expect(section.getByTestId(`ext-client-row-${clientName}`)).toHaveCount(0)

  // ── The same credentials are refused now ───────────────────────────
  const refused = await requestToken(request, tokenUrl, clientId, secret)
  expect(refused.status()).toBe(401)
})
