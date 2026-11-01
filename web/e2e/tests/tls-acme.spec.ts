import { test, expect, type APIRequestContext, type Locator, type Page } from '../harness'
import tls from 'node:tls'

/**
 * E2E for managed certificates + DNS-01 (`TlsManagedCertificates`). The
 * server runs with `PECKBOARD_ACME_TEST=1` (playwright.config.ts), which
 * enables the `mock` DNS provider and replaces the real ACME round trip with
 * a local self-sign — any domain containing `fail` errors instead.
 */

const ADMIN_USER = 'e2e-user'
const ADMIN_PASS = 'e2e-password-1234'
const HTTPS_PORT = Number(process.env.PECKBOARD_E2E_HTTPS_PORT ?? '4445')

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: ADMIN_USER, password: ADMIN_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function openTlsSettings(page: Page, token: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto('/settings')
  await expect(page.getByTestId('settings-page')).toBeVisible({ timeout: 10_000 })
  await page.getByTestId('settings-nav-tls').click()
  await expect(page.getByTestId('tls-certs-section')).toBeVisible({ timeout: 10_000 })
}

/** The `List` row wrapping a name cell with this text. */
function rowFor(page: Page, testId: string, text: string): Locator {
  return page
    .locator('.list-view-row')
    .filter({ has: page.getByTestId(testId).filter({ hasText: text }) })
}

async function openRowMenu(row: Locator) {
  await row.getByRole('button', { name: 'Row menu' }).click()
}

/** Select the DNS credential whose option text contains `label`. */
async function pickCredential(modal: Locator, label: string) {
  const select = modal.getByTestId('tls-cert-dns')
  const value = await select.locator('option', { hasText: label }).getAttribute('value')
  await select.selectOption(value ?? '')
}

/** SANs of the certificate the HTTPS listener serves for `servername`. */
function servedSans(servername: string): Promise<string> {
  return new Promise((resolve, reject) => {
    const sock = tls.connect(
      { host: '127.0.0.1', port: HTTPS_PORT, servername, rejectUnauthorized: false },
      () => {
        const san = sock.getPeerCertificate().subjectaltname ?? ''
        sock.end()
        resolve(san)
      },
    )
    sock.on('error', reject)
  })
}

test.describe('TLS managed certificates (DNS-01)', () => {
  test('add a DNS provider, issue a cert, see a failure, then clean up', async ({
    request,
    page,
  }) => {
    const { token, auth } = await authenticate(request)
    const label = `Mock zone ${Date.now()}`

    try {
      await openTlsSettings(page, token)

      // ── DNS provider ──────────────────────────────────────────────
      await page.getByTestId('tls-add-dns').click()
      const dnsModal = page.getByTestId('tls-dns-modal')
      await expect(dnsModal).toBeVisible()
      await dnsModal.getByTestId('tls-dns-provider').selectOption('mock')
      await dnsModal.getByTestId('tls-dns-label').fill(label)
      await expect(dnsModal.getByTestId('tls-dns-disabled-reason')).toBeVisible()
      await dnsModal.getByTestId('tls-dns-field-token').fill('mock-token-123456')
      await dnsModal.getByTestId('tls-dns-submit').click()
      await expect(dnsModal).toHaveCount(0)
      const dnsRow = rowFor(page, 'tls-dns-row', label)
      await expect(dnsRow).toBeVisible()
      // Secret is masked: last four chars only.
      await expect(dnsRow).toContainText('3456')
      await expect(dnsRow).not.toContainText('mock-token-123456')

      // ── Issued certificate ────────────────────────────────────────
      await page.getByTestId('tls-add-cert').click()
      const certModal = page.getByTestId('tls-add-cert-modal')
      await expect(certModal).toBeVisible()
      await expect(certModal.getByTestId('tls-cert-disabled-reason')).toHaveText(
        'Enter at least one hostname.',
      )
      await certModal.getByTestId('tls-cert-hosts').fill('APP.test.example, app.test.example')
      // Normalised: lowercased + deduped to one chip.
      await expect(certModal.getByTestId('tls-cert-hosts-chips').locator('li')).toHaveText([
        'app.test.example',
      ])
      await pickCredential(certModal, label)
      await certModal.getByTestId('tls-cert-email').fill('admin@test.example')
      await certModal.getByTestId('tls-cert-submit').click()
      await expect(certModal).toHaveCount(0)

      const okRow = rowFor(page, 'tls-cert-row', 'app.test.example')
      await expect(okRow.getByTestId('tls-cert-status')).toHaveText('issued', { timeout: 20_000 })
      await expect(okRow).toContainText("Let's Encrypt")
      await expect(okRow).toContainText('Expires')

      // The HTTPS listener now serves it for that SNI name.
      await expect.poll(() => servedSans('app.test.example')).toContain('app.test.example')

      // ── Failing certificate ───────────────────────────────────────
      await page.getByTestId('tls-add-cert').click()
      await certModal.getByTestId('tls-cert-hosts').fill('fail.test.example')
      await pickCredential(certModal, label)
      await certModal.getByTestId('tls-cert-email').fill('admin@test.example')
      await certModal.getByTestId('tls-cert-submit').click()
      await expect(certModal).toHaveCount(0)

      const failRow = rowFor(page, 'tls-cert-row', 'fail.test.example')
      await expect(failRow.getByTestId('tls-cert-status')).toHaveText('error', { timeout: 20_000 })
      await expect(failRow.getByTestId('tls-cert-error-text')).toContainText(
        'test-mode failure for fail.test.example',
      )

      // ── Credential in use can't be deleted (409) ──────────────────
      await openRowMenu(dnsRow)
      await page.getByTestId('tls-dns-delete').click()
      const dnsConfirm = page.getByTestId('tls-dns-delete-confirm')
      await dnsConfirm.getByTestId('confirm-dialog-confirm').click()
      await expect(dnsConfirm.getByTestId('confirm-dialog-error')).toBeVisible()
      await dnsConfirm.getByTestId('confirm-dialog-cancel').click()

      // ── Delete both certificates, then the credential ─────────────
      for (const row of [failRow, okRow]) {
        await openRowMenu(row)
        await page.getByTestId('tls-cert-delete').click()
        await page
          .getByTestId('tls-cert-delete-confirm')
          .getByTestId('confirm-dialog-confirm')
          .click()
        await expect(row).toHaveCount(0)
      }

      await openRowMenu(dnsRow)
      await page.getByTestId('tls-dns-delete').click()
      await dnsConfirm.getByTestId('confirm-dialog-confirm').click()
      await expect(dnsRow).toHaveCount(0)
    } finally {
      // Best-effort cleanup so re-runs and later specs start clean.
      const res = await request.get('/api/settings/tls', { headers: auth })
      if (res.ok()) {
        const s = (await res.json()) as {
          certificates?: { id: string; domains: string[] }[]
          dns_credentials?: { id: string; label: string }[]
        }
        for (const c of s.certificates ?? []) {
          if (c.domains.some((d) => d.endsWith('test.example'))) {
            await request.delete(`/api/settings/tls/certificates/${c.id}`, { headers: auth })
          }
        }
        for (const c of s.dns_credentials ?? []) {
          if (c.label === label) {
            await request.delete(`/api/settings/tls/dns-credentials/${c.id}`, { headers: auth })
          }
        }
      }
    }
  })
})
