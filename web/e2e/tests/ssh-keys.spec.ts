import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * UI e2e for Settings → SSH Keys (the key vault section).
 *
 * The user-visible flow, end to end against the real REST API:
 * generate a key → the row shows up with its fingerprint → delete it
 * through the 3-dot menu's destructive confirm → the row disappears.
 *
 * Import is deliberately not exercised here: it needs a real private key
 * pasted in, and the parsing it depends on is covered by the Rust tests
 * for `service::ssh_keys::import_private_key`.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const KEY_NAME = 'e2e-ssh-key'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

/** The dev server is reused across local runs, so drop a leftover key
 *  from an earlier run before asserting on the empty-ish list. */
async function wipeKey(request: APIRequestContext, token: string) {
  const res = await request.get('/api/ssh-keys', {
    headers: { Authorization: `Bearer ${token}` },
  })
  expect(res.ok(), `list failed: ${await res.text()}`).toBeTruthy()
  const { keys } = (await res.json()) as { keys: { id: string; name: string }[] }
  for (const key of keys.filter((k) => k.name === KEY_NAME)) {
    await request.delete(`/api/ssh-keys/${key.id}`, {
      headers: { Authorization: `Bearer ${token}` },
    })
  }
}

async function openSshKeySettings(page: Page, token: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/settings')
  await page.getByTestId('settings-nav-ssh-keys').click()
  await expect(page.getByTestId('ssh-keys-section')).toBeVisible({ timeout: 10_000 })
}

test('SSH keys: generate a key, see its fingerprint, delete it', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const token = await authenticate(request)
  await wipeKey(request, token)

  await openSshKeySettings(page, token)

  // Generate — the dialog is a Modal with a <select> for the key type.
  await page.getByTestId('ssh-key-generate-btn').click()
  await expect(page.getByTestId('ssh-key-generate-modal')).toBeVisible()
  const submit = page.getByTestId('ssh-key-generate-submit')
  await expect(submit).toBeDisabled()
  await expect(page.getByTestId('ssh-key-generate-disabled-reason')).toHaveText(
    'Give the key a name.',
  )
  await page.getByTestId('ssh-key-generate-name-input').fill(KEY_NAME)
  await page.getByTestId('ssh-key-generate-type-select').selectOption('ed25519')
  await submit.click()

  await expect(page.getByTestId('ssh-key-generate-modal')).toHaveCount(0)
  await expect(page.getByTestId(`ssh-key-name-${KEY_NAME}`)).toBeVisible()
  await expect(page.getByTestId(`ssh-key-fingerprint-${KEY_NAME}`)).toContainText('SHA256:')

  // Delete through the row's 3-dot menu and its destructive confirm.
  const row = page.locator('.list-view-row', { hasText: KEY_NAME })
  await row.getByRole('button', { name: 'Row menu' }).click()
  await page.getByRole('menuitem', { name: 'Delete' }).click()
  const confirm = page.getByTestId('ssh-key-delete-confirm')
  await expect(confirm).toBeVisible()
  await expect(confirm).toContainText('stops working until it is pointed at another key')
  await page.getByTestId('confirm-dialog-confirm').click()

  await expect(page.getByTestId(`ssh-key-name-${KEY_NAME}`)).toHaveCount(0)
  await expect(page.getByTestId('ssh-keys-section')).toContainText('No SSH keys yet')
})
