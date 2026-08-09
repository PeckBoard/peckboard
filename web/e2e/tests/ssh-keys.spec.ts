import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * UI e2e for the SSH key vault (Settings → SSH Keys).
 *
 * Drives the whole user-visible loop: generate a keypair, confirm the row
 * renders with its fingerprint, read the public key back out of the
 * dialog, then delete it through the confirmation and confirm it's gone.
 * Generation is deterministic server-side (ed25519, no network), so no
 * agent session or provider stub is needed. A second test drives the other
 * entry point — importing a key the user already has — because that path
 * posts a multi-line PEM and only a real browser paste proves the field
 * keeps the newlines.
 */

/**
 * A throwaway ed25519 keypair generated for this test alone (`ssh-keygen -t
 * ed25519 -N ''`). It authenticates nothing anywhere; it exists so the import
 * dialog can be driven with a PEM the server will actually accept.
 */
const FIXTURE_PRIVATE_KEY = `-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACAsD7/AO0o7S5jTyXPfQxkiySDhJ7lHQ19AFf5ZPAmT2AAAAKA+pk6jPqZO
owAAAAtzc2gtZWQyNTUxOQAAACAsD7/AO0o7S5jTyXPfQxkiySDhJ7lHQ19AFf5ZPAmT2A
AAAED0CKlxuTQXUqdt2iQmDEK5hOd9RXxhQYYLvIYP5tya/SwPv8A7SjtLmNPJc99DGSLJ
IOEnuUdDX0AV/lk8CZPYAAAAHHBlY2tib2FyZC1lMmUtaW1wb3J0LWZpeHR1cmUB
-----END OPENSSH PRIVATE KEY-----
`

const FIXTURE_PUBLIC_KEY =
  'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICwPv8A7SjtLmNPJc99DGSLJIOEnuUdDX0AV/lk8CZPY'

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

async function loadApp(page: Page, token: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/')
  await expect(page.locator('.rail-brand')).toBeVisible({ timeout: 10_000 })
}

async function openSshKeys(page: Page) {
  await page.locator('.rail-avatar').click()
  const menu = page.locator('.user-menu-dropdown')
  await expect(menu).toBeVisible()
  await menu.getByRole('menuitem', { name: 'Settings' }).click()
  const settings = page.getByTestId('settings-page')
  await expect(settings).toBeVisible()
  await settings.getByTestId('settings-nav-ssh-keys').click()
  const section = settings.getByTestId('ssh-keys-section')
  await expect(section).toBeVisible()
  return section
}

test('generate an SSH key, read its public half, then delete it', async ({ request, page }) => {
  const token = await authenticate(request)
  await loadApp(page, token)

  const section = await openSshKeys(page)
  await expect(section.getByTestId('ssh-keys-empty')).toBeVisible()

  // ── Generate ───────────────────────────────────────────────────────
  await section.getByTestId('ssh-key-generate').click()
  const modal = page.getByTestId('ssh-key-generate-modal')
  await expect(modal).toBeVisible()
  // The primary action states why it's disabled until the key is named.
  await expect(modal.getByTestId('ssh-generate-disabled-reason')).toBeVisible()
  await expect(modal.getByTestId('ssh-generate-submit')).toBeDisabled()

  await modal.getByTestId('ssh-generate-name').fill('e2e-fleet')
  await expect(modal.getByTestId('ssh-generate-type')).toHaveValue('ed25519')
  await modal.getByTestId('ssh-generate-submit').click()
  await expect(modal).toBeHidden()

  // ── The row carries the name, key type and a real fingerprint ──────
  await expect(section.getByTestId('ssh-key-row-e2e-fleet')).toBeVisible()
  const fingerprint = section.getByTestId('ssh-key-fingerprint-e2e-fleet')
  await expect(fingerprint).toContainText('SHA256:')
  const row = section.locator('.list-view-row').filter({ hasText: 'e2e-fleet' })
  await expect(row).toContainText('ed25519')

  // ── The public key is readable (that's the half users hand out) ────
  await row.locator('.list-view-item').click()
  const publicModal = page.getByTestId('ssh-key-public-modal')
  await expect(publicModal).toBeVisible()
  await expect(publicModal.getByTestId('ssh-key-public-text')).toHaveValue(/^ssh-ed25519 /)
  await publicModal.getByRole('button', { name: 'Close' }).click()
  await expect(publicModal).toBeHidden()

  // ── Delete, via the row's 3-dot menu and the shared confirm ────────
  await row.locator('.list-view-menu').click()
  await page.getByRole('menuitem', { name: 'Delete' }).click()
  const confirm = page.getByTestId('ssh-key-delete-confirm')
  await expect(confirm).toBeVisible()
  // The copy has to warn that hosts using the key will break.
  await expect(confirm).toContainText('stop connecting')
  await confirm.getByTestId('confirm-dialog-confirm').click()

  await expect(section.getByTestId('ssh-key-row-e2e-fleet')).toHaveCount(0)
  await expect(section.getByTestId('ssh-keys-empty')).toBeVisible()
})

test('import an existing private key, pasted as multi-line PEM', async ({ request, page }) => {
  const token = await authenticate(request)
  await loadApp(page, token)

  const section = await openSshKeys(page)
  await section.getByTestId('ssh-key-import').click()
  const modal = page.getByTestId('ssh-key-import-modal')
  await expect(modal).toBeVisible()

  // Nothing typed yet: the primary action says why it's disabled.
  await expect(modal.getByTestId('ssh-import-disabled-reason')).toBeVisible()
  await expect(modal.getByTestId('ssh-import-submit')).toBeDisabled()

  // A PEM that doesn't parse is rejected on the private-key field, not as a
  // raw body and not on the name field.
  await modal.getByTestId('ssh-import-name').fill('e2e-imported')
  await modal.getByTestId('ssh-import-private').fill('not a real key')
  await modal.getByTestId('ssh-import-submit').click()
  await expect(modal.getByTestId('ssh-import-private-error')).toContainText('invalid private key')
  await expect(modal.getByTestId('ssh-import-name-error')).toHaveCount(0)

  // The real thing. `fill` on a <textarea> keeps the newlines a PEM needs;
  // a single-line <input> would drop them and the server would refuse.
  await modal.getByTestId('ssh-import-private').fill(FIXTURE_PRIVATE_KEY)
  await expect(modal.getByTestId('ssh-import-private')).toHaveValue(/BEGIN OPENSSH PRIVATE KEY/)
  await modal.getByTestId('ssh-import-submit').click()
  await expect(modal).toBeHidden()

  // The row shows the imported key, and the public half we get back is the
  // one that belongs to the private key we pasted.
  const row = section.locator('.list-view-row').filter({ hasText: 'e2e-imported' })
  await expect(row).toContainText('ed25519')
  await row.locator('.list-view-item').click()
  const publicModal = page.getByTestId('ssh-key-public-modal')
  await expect(publicModal.getByTestId('ssh-key-public-text')).toHaveValue(
    new RegExp(FIXTURE_PUBLIC_KEY.replace(/[/+]/g, '\\$&')),
  )
  await publicModal.getByRole('button', { name: 'Close' }).click()

  // Clean up so the two tests can share a server.
  await row.locator('.list-view-menu').click()
  await page.getByRole('menuitem', { name: 'Delete' }).click()
  await page.getByTestId('ssh-key-delete-confirm').getByTestId('confirm-dialog-confirm').click()
  await expect(section.getByTestId('ssh-key-row-e2e-imported')).toHaveCount(0)
})
