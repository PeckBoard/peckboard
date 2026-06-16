import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * UI e2e for the plugin hook-approval prompt.
 *
 * A freshly-installed WASM plugin loads **inert** — none of its hooks
 * fire — until an operator approves the exact set of hooks it declares.
 * The backend reports each loaded plugin's approval status in the
 * `/api/plugins` catalog (`wasm_plugins[]`), and `App.tsx` surfaces any
 * with status `pending` as a modal prompt that POSTs the decision to
 * `/api/plugins/:id/approval`.
 *
 * A real `.wasm` going pending → approved can't be compiled in CI (no
 * wasm32 toolchain — see `tests/plugin_http_routes.rs`), so this drives
 * the host plumbing deterministically by mocking the catalog (to report a
 * pending plugin) and the approval endpoint. The backend persistence and
 * gating are covered by `tests/plugin_approvals.rs` and the
 * `src/plugin/manager.rs` unit tests.
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

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

test('approval prompt lists a pending plugin and approves it', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)

  // Mock the catalog so it reports one WASM plugin awaiting approval. The
  // prompt is generic — it surfaces whatever pending plugins the catalog
  // reports and lists exactly the hooks they declare.
  await page.route('**/api/plugins', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        plugins: [],
        ui_panels: [],
        wasm_plugins: [
          {
            name: 'demo',
            hooks: ['http.request.before', 'todo'],
            status: 'pending',
            error: null,
          },
        ],
      }),
    })
  })

  // Capture the approval decision the prompt POSTs.
  let approvalBody: { decision?: string } | null = null
  await page.route('**/api/plugins/*/approval', async (route) => {
    approvalBody = route.request().postDataJSON() as { decision?: string }
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        plugin: { name: 'demo', hooks: ['http.request.before', 'todo'], status: 'approved' },
      }),
    })
  })

  await loadAppAt(page, token, '/')

  // The prompt appears, names the plugin, and lists the hooks it requests.
  const prompt = page.getByTestId('plugin-approval-prompt')
  await expect(prompt).toBeVisible({ timeout: 10_000 })
  await expect(page.getByTestId('plugin-approval-name')).toHaveText('demo')
  const hooks = page.getByTestId('plugin-approval-hooks')
  await expect(hooks).toContainText('http.request.before')
  await expect(hooks).toContainText('todo')

  // Approving posts the decision and dismisses the prompt.
  await page.getByTestId('plugin-approval-approve').click()
  await expect(prompt).toBeHidden()
  expect(approvalBody).not.toBeNull()
  expect(approvalBody!.decision).toBe('approve')
})

test('approval prompt can deny a pending plugin', async ({ request, page, baseURL }) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)

  await page.route('**/api/plugins', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        plugins: [],
        ui_panels: [],
        wasm_plugins: [
          { name: 'demo', hooks: ['mcp.token.issue.before'], status: 'pending', error: null },
        ],
      }),
    })
  })

  let approvalBody: { decision?: string } | null = null
  await page.route('**/api/plugins/*/approval', async (route) => {
    approvalBody = route.request().postDataJSON() as { decision?: string }
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        plugin: { name: 'demo', hooks: ['mcp.token.issue.before'], status: 'denied' },
      }),
    })
  })

  await loadAppAt(page, token, '/')

  const prompt = page.getByTestId('plugin-approval-prompt')
  await expect(prompt).toBeVisible({ timeout: 10_000 })
  await page.getByTestId('plugin-approval-deny').click()
  await expect(prompt).toBeHidden()
  expect(approvalBody).not.toBeNull()
  expect(approvalBody!.decision).toBe('deny')
})
