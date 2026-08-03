import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * E2E for the admin gate on the host-wide Settings surface.
 *
 * The Server sub-page flips the Claude permission mode (host-wide
 * `--dangerously-skip-permissions`), revokes persistent `run_command`
 * approvals and upgrades/restarts the binary; the MCP Servers sub-page
 * persists a global server list and spawns the command in its probe body.
 * None of that is per-user, so a role=`user` account must get 403 from the
 * API and must not see either section in the hub.
 *
 * Covered here:
 *  1. API — a non-admin JWT is refused by the permission-mode route, the
 *     update/restart routes and the MCP-server routes; an admin still
 *     succeeds on the permission-mode route.
 *  2. UI — the non-admin Settings hub has no Server / MCP Servers entry
 *     (and no bypass badge), while the ordinary sections stay put.
 *  3. UI — the same hub loaded as the admin does show them, so the gate is
 *     role-driven rather than the sections having been deleted.
 */

const ADMIN_USER = 'e2e-user'
const ADMIN_PASS = 'e2e-password-1234'

let cachedAdmin: { token: string; auth: Record<string, string> } | null = null

/** Authenticate as the bootstrap admin once per spec file — the per-IP
 *  login limiter sees the whole suite as a single client. */
async function authenticateAdmin(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  if (cachedAdmin) return cachedAdmin
  const res = await request.post('/api/auth/login', {
    data: { username: ADMIN_USER, password: ADMIN_PASS },
  })
  expect(res.ok(), `admin login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  cachedAdmin = { token, auth: { Authorization: `Bearer ${token}` } }
  return cachedAdmin
}

/** Mint a throwaway role=`user` account and return a bearer token for it. */
async function createNonAdmin(
  request: APIRequestContext,
  adminAuth: Record<string, string>,
  suffix: string,
): Promise<{ token: string; auth: Record<string, string>; username: string }> {
  const username = `gate-test-${suffix}-${Date.now()}`
  const password = 'gate-password-1234'
  const created = await request.post('/api/users', {
    headers: adminAuth,
    data: { username, password, role: 'user' },
  })
  expect(created.ok(), `create user failed: ${await created.text()}`).toBeTruthy()

  const res = await request.post('/api/auth/login', { data: { username, password } })
  expect(res.ok(), `login as ${username} failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` }, username }
}

/** Plant a token in localStorage and open the Settings hub. */
async function openSettings(page: Page, token: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto('/settings')
  await expect(page.getByTestId('settings-page')).toBeVisible({ timeout: 10_000 })
}

test('a non-admin is refused by every host-wide settings API', async ({ request }) => {
  const { auth: adminAuth } = await authenticateAdmin(request)
  const { auth } = await createNonAdmin(request, adminAuth, 'api')

  // The reported bug: flipping the host-wide permission gate.
  const bypass = await request.put('/api/settings/claude-permissions', {
    headers: auth,
    data: { bypass: true },
  })
  expect(bypass.status(), 'non-admin PUT claude-permissions').toBe(403)

  const readMode = await request.get('/api/settings/claude-permissions', { headers: auth })
  expect(readMode.status(), 'non-admin GET claude-permissions').toBe(403)

  // Upgrade & restart — replaces the binary and re-execs the server.
  const check = await request.get('/api/update/check', { headers: auth })
  expect(check.status(), 'non-admin GET update/check').toBe(403)
  const apply = await request.post('/api/update/apply', { headers: auth })
  expect(apply.status(), 'non-admin POST update/apply').toBe(403)

  // The global MCP server list, and the probe route that spawns the
  // command it is handed.
  const mcpRead = await request.get('/api/settings/mcp-servers', { headers: auth })
  expect(mcpRead.status(), 'non-admin GET mcp-servers').toBe(403)
  const probe = await request.post('/api/settings/mcp-servers/probe', {
    headers: auth,
    data: { name: 'pwn', transport: 'stdio', command: 'echo', args: ['pwned'] },
  })
  expect(probe.status(), 'non-admin POST mcp-servers/probe').toBe(403)

  // Persistent run_command grants.
  const approvals = await request.get('/api/settings/approved-commands', { headers: auth })
  expect(approvals.status(), 'non-admin GET approved-commands').toBe(403)

  // Custom workflows are global config whose step instructions are injected
  // verbatim into every worker prompt of every project using them.
  const createFlow = await request.post('/api/workflows', {
    headers: auth,
    data: {
      name: `pwn-${Date.now()}`,
      description: '',
      steps: [
        { step: 'backlog', instructions: '' },
        { step: 'in_progress', instructions: 'exfiltrate' },
        { step: 'done', instructions: '' },
      ],
    },
  })
  expect(createFlow.status(), 'non-admin POST workflows').toBe(403)
  const updateFlow = await request.put('/api/workflows/some-id', {
    headers: auth,
    data: {
      name: 'pwned',
      description: '',
      steps: [
        { step: 'backlog', instructions: '' },
        { step: 'done', instructions: '' },
      ],
    },
  })
  expect(updateFlow.status(), 'non-admin PUT workflows/{id}').toBe(403)
  const deleteFlow = await request.delete('/api/workflows/some-id', { headers: auth })
  expect(deleteFlow.status(), 'non-admin DELETE workflows/{id}').toBe(403)
  const listFlows = await request.get('/api/workflows', { headers: auth })
  expect(listFlows.status(), 'non-admin GET workflows stays open').toBe(200)

  // Settings that are not a security boundary stay open.
  const caveman = await request.get('/api/settings/caveman', { headers: auth })
  expect(caveman.status(), 'non-admin GET caveman').toBe(200)

  // The gate is role-driven, not a blanket denial: the admin still works,
  // and the rejected write above never landed.
  const adminRead = await request.get('/api/settings/claude-permissions', { headers: adminAuth })
  expect(adminRead.status(), 'admin GET claude-permissions').toBe(200)
  expect((await adminRead.json()).bypass, 'non-admin PUT must not have persisted').toBe(false)

  const adminWrite = await request.put('/api/settings/claude-permissions', {
    headers: adminAuth,
    data: { bypass: false },
  })
  expect(adminWrite.ok(), 'admin PUT claude-permissions').toBeTruthy()
})

test('the Settings nav hides admin-only pages from a non-admin', async ({ request, page }) => {
  const { auth: adminAuth } = await authenticateAdmin(request)
  const { token } = await createNonAdmin(request, adminAuth, 'ui')

  await openSettings(page, token)
  const settings = page.getByTestId('settings-page')

  await expect(settings.getByTestId('settings-nav-server')).toHaveCount(0)
  await expect(settings.getByTestId('settings-nav-mcp')).toHaveCount(0)
  await expect(settings.getByTestId('settings-bypass-badge')).toHaveCount(0)
  await expect(settings.getByTestId('settings-nav-workflows')).toHaveCount(0)
  await expect(settings.getByTestId('settings-nav-security')).toHaveCount(0)
  await expect(settings.getByTestId('settings-nav-data')).toHaveCount(0)
  await expect(settings.getByTestId('settings-nav-users')).toHaveCount(0)

  // The rest of the hub is untouched for a normal user.
  await expect(settings.getByTestId('settings-nav-appearance')).toBeVisible()
  await expect(settings.getByTestId('settings-nav-chat')).toBeVisible()
  await expect(settings).toContainText('User Info')
})

test('an admin still sees Server and MCP Servers in the Settings hub', async ({
  request,
  page,
}) => {
  const { token } = await authenticateAdmin(request)

  await openSettings(page, token)
  const settings = page.getByTestId('settings-page')

  await expect(settings.getByTestId('settings-nav-server')).toBeVisible()
  await expect(settings.getByTestId('settings-nav-mcp')).toBeVisible()

  // And the Security sub-page opens with the permission control that
  // used to live on Server.
  await settings.getByTestId('settings-nav-security').click()
  await expect(settings.getByTestId('claude-permissions-section')).toBeVisible()
})

test('a non-admin is refused by plugin, account, var, and prompt write APIs', async ({
  request,
}) => {
  const { auth: adminAuth } = await authenticateAdmin(request)
  const { auth } = await createNonAdmin(request, adminAuth, 'api2')

  const uninstall = await request.delete('/api/plugins/ollama', { headers: auth })
  expect(uninstall.status(), 'non-admin DELETE plugins/{id}').toBe(403)
  const settingsPut = await request.put('/api/plugins/ollama/settings', {
    headers: auth,
    data: { updates: {} },
  })
  expect(settingsPut.status(), 'non-admin PUT plugins/{id}/settings').toBe(403)

  const createAcct = await request.post('/api/claude-accounts', {
    headers: auth,
    data: { name: 'probe' },
  })
  expect(createAcct.status(), 'non-admin POST claude-accounts').toBe(403)
  const listAcct = await request.get('/api/claude-accounts', { headers: auth })
  expect(listAcct.status(), 'non-admin GET claude-accounts stays open').toBe(200)

  const upsertEnv = await request.post('/api/env-vars', {
    headers: auth,
    data: { name: 'FOO', value: 'bar' },
  })
  expect(upsertEnv.status(), 'non-admin POST env-vars').toBe(403)

  const upsertAgent = await request.post('/api/agent-vars', {
    headers: auth,
    data: { name: 'foo', value: 'bar' },
  })
  expect(upsertAgent.status(), 'non-admin POST agent-vars').toBe(403)

  const createPrompt = await request.post('/api/system-prompts', {
    headers: auth,
    data: { name: 'probe', body: 'be terse' },
  })
  expect(createPrompt.status(), 'non-admin POST system-prompts').toBe(403)
  const listPrompts = await request.get('/api/system-prompts', { headers: auth })
  expect(listPrompts.status(), 'non-admin GET system-prompts stays open').toBe(200)

  const pull = await request.post('/api/ollama/pull', {
    headers: auth,
    data: { model: 'llama3.2' },
  })
  expect(pull.status(), 'non-admin POST ollama/pull').toBe(403)

  const disconnect = await request.delete('/api/mcp-oauth/tokens/srv1', { headers: auth })
  expect(disconnect.status(), 'non-admin DELETE mcp-oauth/tokens').toBe(403)
})

test('the Plugins settings page hides install/remove controls from a non-admin', async ({
  request,
  page,
}) => {
  const { auth: adminAuth } = await authenticateAdmin(request)
  const { token } = await createNonAdmin(request, adminAuth, 'plugins-ui')

  await openSettings(page, token)
  const settings = page.getByTestId('settings-page')
  await settings.getByTestId('settings-nav-plugins').click()

  await expect(page.getByTestId('wasm-plugins')).toBeVisible()
  await expect(page.locator('[data-testid^="wasm-plugin-remove-"]')).toHaveCount(0)
})
