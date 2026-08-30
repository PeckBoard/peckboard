import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Plugin pages refresh on pushed `plugin-data` WS events instead of polling.
 *
 * Core broadcasts a `plugin-data` frame when a plugin's data store changes;
 * the app forwards it into the sandboxed iframe via postMessage, and the page
 * refetches. The page's own fallback poll is 60s, so an orchestrator created
 * BEHIND the page's back (direct API call, not through its UI) can only
 * appear quickly via the event path — which is exactly what this asserts.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { Authorization: `Bearer ${token}` }
}

async function loginUi(page: Page, baseURL: string) {
  await page.goto(baseURL)
  const username = page.getByLabel('Username')
  await Promise.race([
    username.waitFor({ state: 'visible', timeout: 10_000 }).catch(() => {}),
    page
      .locator('.rail')
      .waitFor({ state: 'visible', timeout: 10_000 })
      .catch(() => {}),
  ])
  if (await username.isVisible().catch(() => false)) {
    await username.fill(E2E_USER)
    await page.getByLabel('Password').fill(E2E_PASS)
    await page.getByRole('button', { name: /sign in/i }).click()
  }
  await expect(page.locator('.rail')).toBeVisible()
}

test('plugin page picks up an API-side change via pushed plugin-data event', async ({
  page,
  baseURL,
  request,
}) => {
  test.setTimeout(120_000)
  expect(baseURL).toBeTruthy()
  const auth = await authenticate(request)

  const catalogRes = await request.get('/api/plugins', { headers: auth })
  const catalog = catalogRes.ok() ? await catalogRes.json() : { plugins: [] }
  test.skip(
    !JSON.stringify(catalog).includes('session-control'),
    'session-control wasm not built/staged — run peck-plugins/session-control/build.sh',
  )

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-push-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-push-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  await loginUi(page, baseURL!)
  await page.getByTestId('plugin-sidebar-session-control-orchestrators').click()
  const frame = page.frameLocator('[data-testid="plugin-fullpage-frame"]')

  // Page booted: its initial fetch ran and the (static) header rendered.
  await expect(frame.getByTestId('orch-new')).toBeVisible({ timeout: 20_000 })
  // Let the page's boot() refresh land before creating anything, so the card
  // below cannot be explained by the initial fetch racing the POST — after
  // this point only the pushed event (or the 60s fallback poll) can surface
  // it. Other specs may have left orchestrators behind (shared e2e server),
  // so all assertions filter by this test's own name.
  await page.waitForTimeout(2000)

  // Create an orchestrator directly through the API — the page knows nothing
  // about it. Its fallback poll is 60s, so appearing within the assertion
  // window below proves the pushed event path end to end.
  const name = `Pushed into view ${Date.now()}`
  const created = await request.post('/api/plugin-ui/session-control/orchestrators', {
    headers: auth,
    data: { name, folder_id: folder.id, goal: 'Prove push refresh works' },
  })
  expect(created.ok(), `create orchestrator failed: ${await created.text()}`).toBeTruthy()

  const card = frame.locator('[data-testid="orch-card"]', { hasText: name })
  await expect(card).toBeVisible({ timeout: 20_000 })
})
