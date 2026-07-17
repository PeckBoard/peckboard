import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * UI e2e for MCP-server OAuth sign-in (Settings → MCP Servers).
 *
 * Servers are seeded through the real settings API; the `/api/mcp-oauth/*`
 * endpoints are mocked at the page level (the real discovery/exchange path
 * needs a live provider and is covered by the Rust `mcp_oauth` integration
 * test). The public `GET /oauth/callback` page is exercised for real — an
 * unknown state renders the "Sign-in expired" page from the actual route.
 *
 * 1. An `auth: "oauth"` server card shows the OAuth badge (sign-in needed).
 * 2. Its editor swaps the Headers rows for the Sign in panel; clicking
 *    Sign in opens the provider tab and the panel flips to Connected once
 *    the poll sees a token; Disconnect flips it back.
 * 3. A provider without dynamic registration (static endpoints, no client
 *    id — the Slack shape) shows client id/secret inputs + redirect URL.
 * 4. The real /oauth/callback route serves the error page for unknown state.
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

/** Mock the /api/mcp-oauth endpoints with a "connects after start" state. */
async function mockOauthApi(page: Page, serverId: string) {
  const state = { connected: false }
  await page.route('**/api/mcp-oauth/start', async (route) => {
    state.connected = true
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({ url: '/oauth/callback?state=e2e-unknown-state&code=fake' }),
    })
  })
  await page.route('**/api/mcp-oauth/tokens', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        tokens: state.connected
          ? {
              [serverId]: {
                server_name: 'linear',
                connected: true,
                expires_at_ms: Date.now() + 3600_000,
                has_refresh_token: true,
              },
            }
          : {},
      }),
    })
  })
  await page.route('**/api/mcp-oauth/tokens/*', async (route) => {
    state.connected = false
    await route.fulfill({ status: 204, body: '' })
  })
  return state
}

test('OAuth server: badge, sign in → connected, disconnect', async ({ request, page, baseURL }) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)

  // Seed one discovery-style OAuth server (registry `oauth: {}` shape).
  const serverId = 'e2e-oauth-srv-1'
  const put = await request.put('/api/settings/mcp-servers', {
    data: {
      servers: [
        {
          id: serverId,
          name: 'linear',
          transport: 'http',
          command: '',
          args: [],
          env: [],
          url: 'https://mcp.example.com/mcp',
          headers: [],
          url_options: [
            { label: 'US1', url: 'https://mcp.example.com/mcp' },
            { label: 'EU', url: 'https://mcp.example.eu/mcp' },
          ],
          auth: 'oauth',
          oauth: {},
          enabled: true,
          providers: [],
          disabled_tools: [],
        },
      ],
    },
    headers: { Authorization: `Bearer ${token}` },
  })
  expect(put.ok(), `seed failed: ${await put.text()}`).toBeTruthy()

  await mockOauthApi(page, serverId)
  await openMcpSettings(page, token)

  // Card badge: OAuth server, not yet signed in.
  const badge = page.getByTestId('mcp-oauth-badge-linear')
  await expect(badge).toBeVisible()
  await expect(badge).toContainText('sign in needed')

  // The editor swaps Headers for the Sign in panel.
  await page.getByTestId('mcp-server-card-linear').getByRole('button', { name: 'Edit' }).click()
  await expect(page.getByTestId('mcp-server-modal')).toBeVisible()
  await expect(page.getByTestId('mcp-oauth-panel')).toBeVisible()
  await expect(page.getByTestId('mcp-oauth-signin')).toBeVisible()
  await expect(page.getByText('Headers', { exact: true })).toHaveCount(0)
  // Discovery-capable template: no client credential inputs up front.
  await expect(page.getByTestId('mcp-oauth-client-id')).toHaveCount(0)

  // Region dropdown (url_options) drives the URL; the callback URL to
  // allow-list is always visible.
  const region = page.getByTestId('mcp-field-url-option')
  await expect(region).toBeVisible()
  await region.selectOption('https://mcp.example.eu/mcp')
  await expect(page.getByTestId('mcp-field-url')).toHaveValue('https://mcp.example.eu/mcp')
  await region.selectOption('https://mcp.example.com/mcp')
  await expect(page.getByTestId('mcp-oauth-callback')).toContainText('/oauth/callback')

  // Extra sign-in parameter rows (SSO hints like Slack team=…).
  await page.getByTestId('mcp-oauth-add-param').click()
  await page.getByTestId('mcp-oauth-param-key-0').fill('team')

  await page.waitForTimeout(400)
  await page.screenshot({ path: 'e2e/test-results/mcp-oauth-signin-panel.png' })

  // Sign in: opens the provider tab (here: the real callback page with an
  // unknown state — the actual public route), then the poll sees the token.
  const popupPromise = page.waitForEvent('popup')
  await page.getByTestId('mcp-oauth-signin').click()
  const popup = await popupPromise
  await expect(popup.getByText('Sign-in expired')).toBeVisible({ timeout: 10_000 })
  await popup.screenshot({ path: 'e2e/test-results/mcp-oauth-callback-expired.png' })
  await popup.close()

  await expect(page.getByTestId('mcp-oauth-status')).toContainText('Connected', {
    timeout: 15_000,
  })
  await page.screenshot({ path: 'e2e/test-results/mcp-oauth-connected.png' })

  // Close the editor: the card badge now shows connected.
  await page.keyboard.press('Escape')
  await expect(page.getByTestId('mcp-server-modal')).toHaveCount(0)
  await expect(page.getByTestId('mcp-oauth-badge-linear')).toContainText('OAuth ✓')
  await page.screenshot({ path: 'e2e/test-results/mcp-oauth-card-connected.png', fullPage: true })

  // Disconnect from the editor flips back to the sign-in offer.
  await page.getByTestId('mcp-server-card-linear').getByRole('button', { name: 'Edit' }).click()
  await expect(page.getByTestId('mcp-oauth-status')).toBeVisible()
  await page.getByTestId('mcp-oauth-disconnect').click()
  await expect(page.getByTestId('mcp-oauth-signin')).toBeVisible()

  // Manual-headers escape hatch swaps the panel back to header rows.
  await page.getByTestId('mcp-oauth-manual').click()
  await expect(page.getByTestId('mcp-oauth-panel')).toHaveCount(0)
  await expect(page.getByText('Headers', { exact: true })).toBeVisible()
  await expect(page.getByTestId('mcp-oauth-use')).toBeVisible()
  await page.keyboard.press('Escape')

  // Clean up the seeded list.
  const wipe = await request.put('/api/settings/mcp-servers', {
    data: { servers: [] },
    headers: { Authorization: `Bearer ${token}` },
  })
  expect(wipe.ok()).toBeTruthy()
})

test('provider without dynamic registration asks for client credentials', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const serverId = 'e2e-oauth-srv-2'
  const put = await request.put('/api/settings/mcp-servers', {
    data: {
      servers: [
        {
          id: serverId,
          name: 'slack',
          transport: 'http',
          command: '',
          args: [],
          env: [],
          url: 'https://mcp.example.com/mcp',
          headers: [],
          auth: 'oauth',
          // Slack shape: static endpoints, no registration, no client id.
          oauth: {
            authorize_url: 'https://slack.example/oauth/v2_user/authorize',
            token_url: 'https://slack.example/api/oauth.v2.user.access',
            scope_param: 'user_scope',
            token_field: 'authed_user.access_token',
          },
          enabled: true,
          providers: [],
          disabled_tools: [],
        },
      ],
    },
    headers: { Authorization: `Bearer ${token}` },
  })
  expect(put.ok(), `seed failed: ${await put.text()}`).toBeTruthy()

  await mockOauthApi(page, serverId)
  // Registry mock: a template whose url_options cover this server's URL —
  // the editor backfills the Region dropdown for servers saved before
  // url_options existed.
  await page.route('**/api/plugins/registry', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        repositories: [],
        plugins: [],
        mcp_servers: [
          {
            id: 'slack',
            name: 'Slack',
            description: '',
            author: '',
            transport: 'http',
            command: '',
            args: [],
            env: [],
            url: 'https://mcp.example.com/mcp',
            headers: [],
            url_options: [
              { label: 'US', url: 'https://mcp.example.com/mcp' },
              { label: 'EU', url: 'https://mcp.example.eu/mcp' },
            ],
            repository: 'x',
            repository_label: 'x',
          },
        ],
      }),
    })
  })
  await openMcpSettings(page, token)
  await page.getByTestId('mcp-server-card-slack').getByRole('button', { name: 'Edit' }).click()

  // Fallback Region dropdown from the registry template (the server itself
  // was saved without url_options).
  await expect(page.getByTestId('mcp-field-url-option')).toBeVisible()

  // Client credentials + the redirect URL to register, shown up front.
  await expect(page.getByTestId('mcp-oauth-client-id')).toBeVisible()
  await expect(page.getByTestId('mcp-oauth-client-secret')).toBeVisible()
  await expect(page.getByTestId('mcp-oauth-panel')).toContainText('/oauth/callback')
  await page.waitForTimeout(400)
  await page.screenshot({ path: 'e2e/test-results/mcp-oauth-needs-client.png' })
  await page.keyboard.press('Escape')

  const wipe = await request.put('/api/settings/mcp-servers', {
    data: { servers: [] },
    headers: { Authorization: `Bearer ${token}` },
  })
  expect(wipe.ok()).toBeTruthy()
})

test('the public /oauth/callback route serves the error page directly', async ({ page }) => {
  // No auth token on purpose — the route is public; the state value is the
  // only capability that can claim a login.
  await page.goto('/oauth/callback?state=definitely-unknown&code=x')
  await expect(page.getByText('Sign-in expired')).toBeVisible()
  await page.goto('/oauth/callback?error=access_denied&error_description=user+cancelled')
  await expect(page.getByText('Sign-in failed')).toBeVisible()
  await expect(page.getByText('access_denied')).toBeVisible()
})
