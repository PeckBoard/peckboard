import { test, expect, type APIRequestContext } from '../harness'

/**
 * UI e2e for installable bundled crate plugins: removing one from
 * Settings → Plugins deactivates it (its models leave the catalog), the
 * registry browser then offers Install (activation, no download), and
 * installing brings the models back.
 *
 * Uses `kimi` — one seed model, and restored via API in afterEach so a
 * mid-test failure can't leave later specs in this shard without it.
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

async function modelIds(request: APIRequestContext, token: string): Promise<string[]> {
  const res = await request.get('/api/models', {
    headers: { Authorization: `Bearer ${token}` },
  })
  expect(res.ok()).toBeTruthy()
  const body = (await res.json()) as { models: { id: string }[] }
  return body.models.map((m) => m.id)
}

test.afterEach(async ({ request }) => {
  // Idempotent restore: whatever happened above, kimi must be installed
  // again for the rest of the shard.
  const token = await authenticate(request)
  await request.post('/api/plugins/registry/install', {
    headers: { Authorization: `Bearer ${token}` },
    data: { id: 'kimi' },
  })
})

test('a bundled crate plugin removes and reinstalls from the registry', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)

  expect((await modelIds(request, token)).some((id) => id.startsWith('kimi:'))).toBe(true)

  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto('/plugins')
  const section = page.getByTestId('plugins-section')
  await expect(section).toBeVisible({ timeout: 10_000 })

  // Remove kimi from its details modal, through the confirm dialog.
  await page.getByTestId('plugin-open-kimi').click()
  const details = page.getByTestId('plugin-details-kimi')
  await expect(details).toBeVisible()
  await details.getByTestId('plugin-remove-kimi').click()
  const confirm = page.locator('.confirm-dialog')
  await expect(confirm).toBeVisible()
  await confirm.getByRole('button', { name: 'Remove' }).click()

  // The row leaves the installed list and the models leave the catalog.
  await expect(page.getByTestId('plugin-card-kimi')).toHaveCount(0)
  await expect
    .poll(async () => (await modelIds(request, token)).some((id) => id.startsWith('kimi:')))
    .toBe(false)

  // The registry browser offers Install (activation of the compiled-in
  // plugin — crate rows are injected even when remote repos are down).
  await page.getByTestId('browse-plugins').click()
  const install = page.getByTestId('registry-install-kimi')
  await expect(install).toBeVisible({ timeout: 15_000 })
  await expect(install).toHaveAttribute('data-action', 'install')
  await install.click()

  // Models return without any download, and the row is back.
  await expect
    .poll(async () => (await modelIds(request, token)).some((id) => id.startsWith('kimi:')))
    .toBe(true)
  await expect(page.getByTestId('registry-install-kimi')).toHaveAttribute(
    'data-action',
    'bundled',
    { timeout: 15_000 },
  )
})
