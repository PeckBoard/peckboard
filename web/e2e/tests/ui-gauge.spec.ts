import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * ui-gauge plugin (staged + approved by the e2e harness):
 *
 *  - The global sidebar offers a "UI Gauge" page served by the plugin.
 *  - The generate-a-baseline controls render (folder + model pickers, the
 *    Generate button) along with the living overall baseline prompt, which
 *    starts at its "no validated directives" state.
 *  - The six default categories render in the editor; a bar override
 *    saves and becomes the effective bar.
 *  - Uploading a baseline screenshot (downscaled client-side) with 1-10
 *    rankings adds it to the gallery, image and all.
 *  - The evaluation history starts empty with its explainer.
 *
 * The scoring pipeline and the generation/overall-prompt logic
 * (rubric → judge → subpar → cards; high ratings → overall prompt) are
 * unit-tested in peck-plugins/ui-gauge; this spec proves the page, the
 * authed routes, and baseline storage end to end.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

// A 1x1 transparent PNG — the page's canvas pipeline downscales/encodes it.
const TINY_PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==',
  'base64',
)

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

test('ui-gauge page: categories, bar override, and a ranked baseline', async ({
  page,
  baseURL,
  request,
}) => {
  expect(baseURL).toBeTruthy()
  const auth = await authenticate(request)

  const catalogRes = await request.get('/api/plugins', { headers: auth })
  const catalog = catalogRes.ok() ? await catalogRes.json() : { plugins: [] }
  test.skip(
    !JSON.stringify(catalog).includes('ui-gauge'),
    'ui-gauge wasm not built/staged — run peck-plugins/ui-gauge/build.sh',
  )

  await loginUi(page, baseURL!)
  await page.getByTestId('plugin-sidebar-ui-gauge-ui-gauge').click()
  const frame = page.frameLocator('[data-testid="plugin-fullpage-frame"]')

  // Generation controls + the overall prompt panel render; no directives yet.
  await expect(frame.getByTestId('gauge-generate')).toBeVisible({ timeout: 15_000 })
  await expect(frame.getByTestId('gauge-gen-folder')).toBeVisible()
  await expect(frame.getByTestId('gauge-gen-model')).toBeVisible()
  await expect(frame.getByTestId('gauge-overall')).toContainText('No validated directives')

  // The six default categories render in the editor.
  await expect(frame.locator('[data-cat-key]')).toHaveCount(6, { timeout: 15_000 })
  await expect(frame.locator('[data-cat-key="0"]')).toHaveValue('visual_hierarchy')

  // Override the first category's bar to 9; the effective bar follows.
  await frame.locator('[data-cat-bar="0"]').fill('9')
  await frame.getByTestId('gauge-save-cats').click()
  await expect(frame.locator('#cats tr').nth(1).locator('b')).toHaveText('9', {
    timeout: 10_000,
  })

  // Add a baseline: screenshot + name + default rankings.
  await frame.getByTestId('gauge-file').setInputFiles({
    name: 'reference.png',
    mimeType: 'image/png',
    buffer: TINY_PNG,
  })
  await frame.getByTestId('gauge-base-name').fill('Settings page — good reference')
  await frame.getByTestId('gauge-base-save').click()

  const baseline = frame.getByTestId('gauge-baseline')
  await expect(baseline).toBeVisible({ timeout: 15_000 })
  await expect(baseline).toContainText('Settings page — good reference')
  // The stored image round-trips back into the gallery.
  await expect(baseline.locator('img')).toHaveAttribute('src', /^data:image\/jpeg;base64,/, {
    timeout: 10_000,
  })

  // History starts empty with its explainer.
  await expect(frame.locator('#evals')).toContainText('ui_gauge_score')
})
