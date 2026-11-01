import { useEffect, useRef, useState, type FormEvent } from 'react'
import { readFileAsText } from '../lib/readFileAsText'
import { authedFetch } from '../store/auth'
import type { DnsCredential, DnsProviderSpec, TlsCertificate, TlsStatus } from '../types/api'
import ConfirmDialog from './ConfirmDialog'
import FieldError from './FieldError'
import List from './List'
import Modal from './Modal'
import SecretInput from './SecretInput'
import type { MenuItem } from './Dropdown'

function formatExpiry(at: string): string {
  const d = new Date(at)
  return isNaN(d.getTime()) ? at : d.toLocaleString()
}

/** JSON call against the TLS admin routes. Every error answers
 *  `{ error }`; a 204 resolves to `null`. */
async function tlsCall<T>(path: string, method: string, body?: unknown): Promise<T | null> {
  const res = await authedFetch(path, {
    method,
    headers: body === undefined ? undefined : { 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
  const data = (await res.json().catch(() => null)) as (T & { error?: string }) | null
  if (!res.ok) throw new Error(data?.error || `HTTP ${res.status}`)
  return data
}

const errMsg = (e: unknown, fallback: string) => (e instanceof Error ? e.message : fallback)

const HOST_RE = /^(\*\.)?([a-z0-9]([a-z0-9-]*[a-z0-9])?\.)*[a-z0-9]([a-z0-9-]*[a-z0-9])?$/

/** Hostnames as the server will store them: split on commas / whitespace,
 *  trimmed, lowercased, deduped. */
function normalizeHostnames(raw: string): string[] {
  const out: string[] = []
  for (const part of raw.split(/[\s,]+/)) {
    const h = part.trim().toLowerCase().replace(/\.$/, '')
    if (h && !out.includes(h)) out.push(h)
  }
  return out
}

const STATUS_CLASS: Record<TlsCertificate['status'], string> = {
  issued: 'status-active',
  pending: 'status-paused',
  error: 'status-failed',
}

/** A PEM textarea with a "Choose file…" picker — same shape as the legacy
 *  upload section's fields. */
function PemField({
  id,
  label,
  placeholder,
  accept,
  value,
  onChange,
  onReadError,
  testId,
}: {
  id: string
  label: string
  placeholder: string
  accept: string
  value: string
  onChange: (pem: string) => void
  onReadError: (msg: string) => void
  testId: string
}) {
  const fileRef = useRef<HTMLInputElement>(null)
  return (
    <div className="form-field">
      <label className="form-label" htmlFor={id}>
        {label}
      </label>
      <textarea
        id={id}
        className="tls-pem-input"
        placeholder={placeholder}
        value={value}
        spellCheck={false}
        onChange={(e) => onChange(e.target.value)}
        data-testid={testId}
      />
      <input
        ref={fileRef}
        type="file"
        accept={accept}
        hidden
        onChange={(e) => {
          const file = e.target.files?.[0]
          e.target.value = ''
          if (!file) return
          readFileAsText(file).then(onChange, () => onReadError(`Could not read ${file.name}.`))
        }}
        data-testid={`${testId}-file`}
      />
      <button type="button" className="btn-secondary" onClick={() => fileRef.current?.click()}>
        Choose file…
      </button>
    </div>
  )
}

function AddDnsCredentialModal({
  providers,
  onClose,
  onCreated,
}: {
  providers: DnsProviderSpec[]
  onClose: () => void
  onCreated: (cred: DnsCredential) => void
}) {
  const [providerId, setProviderId] = useState(providers[0]?.id ?? '')
  const [label, setLabel] = useState('')
  const [fields, setFields] = useState<Record<string, string>>({})
  const [busy, setBusy] = useState(false)
  const [formError, setFormError] = useState('')

  const spec = providers.find((p) => p.id === providerId)
  const missing = spec?.fields.find((f) => f.required && !fields[f.key]?.trim())
  const disabledReason = !spec
    ? 'Pick a DNS provider.'
    : !label.trim()
      ? 'Enter a label.'
      : missing
        ? `Enter ${missing.label}.`
        : null

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    if (disabledReason || busy || !spec) return
    setBusy(true)
    setFormError('')
    try {
      const payload: Record<string, string> = {}
      for (const f of spec.fields) {
        const v = fields[f.key]?.trim()
        if (v) payload[f.key] = v
      }
      const cred = await tlsCall<DnsCredential>('/api/settings/tls/dns-credentials', 'POST', {
        provider: spec.id,
        label: label.trim(),
        fields: payload,
      })
      if (cred) onCreated(cred)
      onClose()
    } catch (err) {
      setFormError(errMsg(err, 'Could not save the DNS provider'))
      setBusy(false)
    }
  }

  return (
    <Modal onClose={busy ? undefined : onClose} maxWidth={520} data-testid="tls-dns-modal">
      <h2>Add DNS Provider</h2>
      <p className="form-hint">
        Credentials Peckboard uses to publish the temporary <code>_acme-challenge</code> TXT record
        that proves you control a domain. Use a token scoped to DNS edits on the zones you need.
      </p>
      <form onSubmit={(e) => void submit(e)}>
        <div className="form-field">
          <label className="form-label" htmlFor="tls-dns-provider">
            Provider
          </label>
          <select
            id="tls-dns-provider"
            className="form-input"
            value={providerId}
            onChange={(e) => {
              setProviderId(e.target.value)
              setFields({})
            }}
            data-testid="tls-dns-provider"
          >
            {providers.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </select>
          {spec?.docs_url && (
            <a
              className="form-hint"
              href={spec.docs_url}
              target="_blank"
              rel="noreferrer"
              data-testid="tls-dns-docs"
            >
              How to create {spec.name} credentials
            </a>
          )}
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="tls-dns-label">
            Label
          </label>
          <input
            id="tls-dns-label"
            className="form-input"
            type="text"
            value={label}
            autoComplete="off"
            placeholder="e.g. Home zone"
            onChange={(e) => setLabel(e.target.value)}
            data-testid="tls-dns-label"
          />
        </div>
        {spec?.fields.map((f) => (
          <div className="form-field" key={f.key}>
            <label className="form-label" htmlFor={`tls-dns-field-${f.key}`}>
              {f.label}
              {!f.required && ' (optional)'}
            </label>
            {f.secret ? (
              <SecretInput
                id={`tls-dns-field-${f.key}`}
                className="form-input"
                label={f.label}
                value={fields[f.key] ?? ''}
                onChange={(v) => setFields((s) => ({ ...s, [f.key]: v }))}
                testId={`tls-dns-field-${f.key}`}
              />
            ) : (
              <input
                id={`tls-dns-field-${f.key}`}
                className="form-input"
                type="text"
                autoComplete="off"
                value={fields[f.key] ?? ''}
                onChange={(e) => setFields((s) => ({ ...s, [f.key]: e.target.value }))}
                data-testid={`tls-dns-field-${f.key}`}
              />
            )}
            {f.help && <span className="form-hint">{f.help}</span>}
          </div>
        ))}
        <FieldError message={formError} testId="tls-dns-error" />
        <div className="form-actions">
          {!busy && disabledReason && (
            <span className="form-actions-reason" data-testid="tls-dns-disabled-reason">
              {disabledReason}
            </span>
          )}
          <button type="button" className="btn-secondary" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={busy || !!disabledReason}
            data-testid="tls-dns-submit"
          >
            {busy ? 'Saving…' : 'Add provider'}
          </button>
        </div>
      </form>
    </Modal>
  )
}

function TestDnsCredentialModal({ cred, onClose }: { cred: DnsCredential; onClose: () => void }) {
  const [domain, setDomain] = useState('')
  const [busy, setBusy] = useState(false)
  const [result, setResult] = useState<{ ok: boolean; message: string } | null>(null)
  const disabledReason = !domain.trim() ? 'Enter a domain to test against.' : null

  const run = async (e: FormEvent) => {
    e.preventDefault()
    if (disabledReason || busy) return
    setBusy(true)
    setResult(null)
    try {
      await tlsCall(`/api/settings/tls/dns-credentials/${cred.id}/test`, 'POST', {
        domain: domain.trim().toLowerCase(),
      })
      setResult({ ok: true, message: 'Created and removed a test TXT record.' })
    } catch (err) {
      setResult({ ok: false, message: errMsg(err, 'Test failed') })
    } finally {
      setBusy(false)
    }
  }

  return (
    <Modal onClose={busy ? undefined : onClose} maxWidth={480} data-testid="tls-dns-test-modal">
      <h2>Test {cred.label}</h2>
      <p className="form-hint">
        Creates and then deletes a <code>_acme-challenge.&lt;domain&gt;</code> TXT record to confirm
        the credentials can edit that zone.
      </p>
      <form onSubmit={(e) => void run(e)}>
        <div className="form-field">
          <label className="form-label" htmlFor="tls-dns-test-domain">
            Domain
          </label>
          <input
            id="tls-dns-test-domain"
            className="form-input"
            type="text"
            value={domain}
            autoComplete="off"
            placeholder="example.com"
            onChange={(e) => {
              setDomain(e.target.value)
              setResult(null)
            }}
            autoFocus
            data-testid="tls-dns-test-domain"
          />
        </div>
        {result &&
          (result.ok ? (
            <p className="form-hint" data-testid="tls-dns-test-ok">
              {result.message}
            </p>
          ) : (
            <FieldError message={result.message} testId="tls-dns-test-error" />
          ))}
        <div className="form-actions">
          {!busy && disabledReason && <span className="form-actions-reason">{disabledReason}</span>}
          <button type="button" className="btn-secondary" onClick={onClose} disabled={busy}>
            Close
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={busy || !!disabledReason}
            data-testid="tls-dns-test-run"
          >
            {busy ? 'Testing…' : 'Run test'}
          </button>
        </div>
      </form>
    </Modal>
  )
}

function AddCertificateModal({
  credentials,
  providers,
  onClose,
  onCreated,
}: {
  credentials: DnsCredential[]
  providers: DnsProviderSpec[]
  onClose: () => void
  /** Re-fetch status after a certificate or credential was created. */
  onCreated: () => void
}) {
  const [source, setSource] = useState<'acme' | 'uploaded'>('acme')
  const [hostsRaw, setHostsRaw] = useState('')
  const [pickedCredId, setCredId] = useState('')
  const [email, setEmail] = useState('')
  const [staging, setStaging] = useState(false)
  const [certPem, setCertPem] = useState('')
  const [keyPem, setKeyPem] = useState('')
  const [addingDns, setAddingDns] = useState(false)
  const [busy, setBusy] = useState(false)
  const [formError, setFormError] = useState('')
  const [pemError, setPemError] = useState('')

  // A credential added from inside this dialog lands in `credentials` on
  // the next status fetch; fall back to the first one until the pick is in
  // the list.
  const credId = credentials.some((c) => c.id === pickedCredId)
    ? pickedCredId
    : (credentials[0]?.id ?? '')

  const hosts = normalizeHostnames(hostsRaw)
  const badHost = hosts.find((h) => !HOST_RE.test(h))
  const emailError = email.trim() && !/^[^\s@]+@[^\s@]+$/.test(email.trim())

  const disabledReason =
    source === 'acme'
      ? hosts.length === 0
        ? 'Enter at least one hostname.'
        : badHost
          ? 'Fix the invalid hostname.'
          : !credId
            ? 'Add a DNS provider first.'
            : !email.trim()
              ? 'Enter a contact email.'
              : emailError
                ? 'Fix the email address.'
                : null
      : !certPem.trim()
        ? 'Paste or choose the certificate.'
        : !keyPem.trim()
          ? 'Paste or choose the private key.'
          : null

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    if (disabledReason || busy) return
    setBusy(true)
    setFormError('')
    try {
      const body =
        source === 'acme'
          ? {
              source,
              domains: hosts,
              dns_credential_id: credId,
              acme_email: email.trim(),
              staging,
            }
          : { source, cert_pem: certPem, key_pem: keyPem }
      await tlsCall<TlsCertificate>('/api/settings/tls/certificates', 'POST', body)
      // Drop key material from the DOM as soon as the server has it.
      setKeyPem('')
      onCreated()
      onClose()
    } catch (err) {
      setFormError(errMsg(err, 'Could not add the certificate'))
      setBusy(false)
    }
  }

  return (
    <Modal onClose={busy ? undefined : onClose} maxWidth={560} data-testid="tls-add-cert-modal">
      <h2>Add Certificate</h2>
      <form onSubmit={(e) => void submit(e)}>
        <div className="form-field">
          <label className="form-label" htmlFor="tls-cert-source">
            Source
          </label>
          <select
            id="tls-cert-source"
            className="form-input"
            value={source}
            onChange={(e) => {
              setSource(e.target.value as 'acme' | 'uploaded')
              setFormError('')
            }}
            data-testid="tls-cert-source"
          >
            <option value="acme">Let&apos;s Encrypt (DNS-01)</option>
            <option value="uploaded">Upload PEM</option>
          </select>
        </div>

        {source === 'acme' ? (
          <>
            <div className="form-field">
              <label className="form-label" htmlFor="tls-cert-hosts">
                Hostnames
              </label>
              <textarea
                id="tls-cert-hosts"
                className="form-input"
                rows={2}
                value={hostsRaw}
                spellCheck={false}
                placeholder="app.example.com, *.example.com"
                onChange={(e) => setHostsRaw(e.target.value)}
                data-testid="tls-cert-hosts"
              />
              <span className="form-hint">Separate with commas or new lines.</span>
              {hosts.length > 0 && (
                <ul className="tls-san-list" data-testid="tls-cert-hosts-chips">
                  {hosts.map((h) => (
                    <li className="tls-san" key={h}>
                      {h}
                    </li>
                  ))}
                </ul>
              )}
              <FieldError
                message={
                  badHost
                    ? `"${badHost}" is not a valid hostname (a wildcard is only allowed as the first label, e.g. *.example.com).`
                    : undefined
                }
                testId="tls-cert-hosts-error"
              />
            </div>
            <div className="form-field">
              <label className="form-label" htmlFor="tls-cert-dns">
                DNS provider
              </label>
              {credentials.length === 0 ? (
                <p className="form-hint" data-testid="tls-cert-no-dns">
                  No DNS provider saved yet.{' '}
                  <button
                    type="button"
                    className="btn-secondary btn-sm"
                    onClick={() => setAddingDns(true)}
                    data-testid="tls-cert-add-dns"
                  >
                    Add DNS provider
                  </button>
                </p>
              ) : (
                <select
                  id="tls-cert-dns"
                  className="form-input"
                  value={credId}
                  onChange={(e) => setCredId(e.target.value)}
                  data-testid="tls-cert-dns"
                >
                  {credentials.map((c) => (
                    <option key={c.id} value={c.id}>
                      {c.label} ({providers.find((p) => p.id === c.provider)?.name ?? c.provider})
                    </option>
                  ))}
                </select>
              )}
            </div>
            <div className="form-field">
              <label className="form-label" htmlFor="tls-cert-email">
                Contact email
              </label>
              <input
                id="tls-cert-email"
                className="form-input"
                type="email"
                value={email}
                autoComplete="email"
                placeholder="you@example.com"
                onChange={(e) => setEmail(e.target.value)}
                data-testid="tls-cert-email"
              />
              <span className="form-hint">Registered with the Let&apos;s Encrypt account.</span>
              <FieldError
                message={emailError ? 'Enter a valid email address.' : undefined}
                testId="tls-cert-email-error"
              />
            </div>
            <label className="confirm-dialog-checkbox">
              <input
                type="checkbox"
                checked={staging}
                onChange={(e) => setStaging(e.target.checked)}
                data-testid="tls-cert-staging"
              />
              <span>Use staging (untrusted test certificates, generous rate limits)</span>
            </label>
          </>
        ) : (
          <>
            <PemField
              id="tls-cert-cert-pem"
              label="Certificate (PEM)"
              placeholder="-----BEGIN CERTIFICATE-----"
              accept=".pem,.crt,.cer,.txt"
              value={certPem}
              onChange={(v) => {
                setPemError('')
                setCertPem(v)
              }}
              onReadError={setPemError}
              testId="tls-cert-cert-pem"
            />
            <PemField
              id="tls-cert-key-pem"
              label="Private key (PEM)"
              placeholder="-----BEGIN PRIVATE KEY-----"
              accept=".pem,.key,.txt"
              value={keyPem}
              onChange={(v) => {
                setPemError('')
                setKeyPem(v)
              }}
              onReadError={setPemError}
              testId="tls-cert-key-pem"
            />
            <FieldError message={pemError} testId="tls-cert-pem-error" />
          </>
        )}

        <FieldError message={formError} testId="tls-cert-error" />
        <div className="form-actions">
          {!busy && disabledReason && (
            <span className="form-actions-reason" data-testid="tls-cert-disabled-reason">
              {disabledReason}
            </span>
          )}
          <button type="button" className="btn-secondary" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={busy || !!disabledReason}
            data-testid="tls-cert-submit"
          >
            {busy ? 'Adding…' : source === 'acme' ? 'Request certificate' : 'Add certificate'}
          </button>
        </div>
      </form>
      {addingDns && (
        <AddDnsCredentialModal
          providers={providers}
          onClose={() => setAddingDns(false)}
          onCreated={(cred) => {
            setCredId(cred.id)
            onCreated()
          }}
        />
      )}
    </Modal>
  )
}

/**
 * Managed certificates (Let's Encrypt via DNS-01, or uploaded) and the DNS
 * provider credentials ACME issuance uses. Rendered by
 * `TlsSettingsSection`, which owns the `/api/settings/tls` status these
 * lists come from; every mutation here calls `reload` afterwards.
 */
export default function TlsManagedCertificates({
  status,
  reload,
}: {
  status: TlsStatus
  reload: () => Promise<void>
}) {
  const certs = status.certificates ?? []
  const creds = status.dns_credentials ?? []
  const providers = status.dns_providers ?? []

  const [addingCert, setAddingCert] = useState(false)
  const [addingDns, setAddingDns] = useState(false)
  const [testing, setTesting] = useState<DnsCredential | null>(null)
  const [deleteCert, setDeleteCert] = useState<TlsCertificate | null>(null)
  const [deleteCred, setDeleteCred] = useState<DnsCredential | null>(null)
  const [deleteBusy, setDeleteBusy] = useState(false)
  const [deleteError, setDeleteError] = useState<string | null>(null)
  const [actionError, setActionError] = useState<string | null>(null)

  // Issuance runs in the background; poll while any certificate is pending.
  const anyPending = certs.some((c) => c.status === 'pending')
  useEffect(() => {
    if (!anyPending) return
    const t = setInterval(() => void reload(), 3000)
    return () => clearInterval(t)
  }, [anyPending, reload])

  const providerName = (id: string) => providers.find((p) => p.id === id)?.name ?? id

  const runDelete = async (path: string, close: () => void) => {
    setDeleteBusy(true)
    setDeleteError(null)
    try {
      await tlsCall(path, 'DELETE')
      close()
      await reload()
    } catch (e) {
      setDeleteError(errMsg(e, 'Delete failed'))
    } finally {
      setDeleteBusy(false)
    }
  }

  const certMenu = (c: TlsCertificate): MenuItem[] => [
    {
      label: 'Renew',
      hidden: c.source !== 'acme',
      disabled: c.status === 'pending',
      onSelect: () => {
        setActionError(null)
        tlsCall(`/api/settings/tls/certificates/${c.id}/renew`, 'POST')
          .then(() => reload())
          .catch((e: unknown) => setActionError(errMsg(e, 'Renew failed')))
      },
      testId: 'tls-cert-renew',
    },
    { divider: true, hidden: c.source !== 'acme' },
    {
      label: 'Delete',
      danger: true,
      onSelect: () => {
        setDeleteError(null)
        setDeleteCert(c)
      },
      testId: 'tls-cert-delete',
    },
  ]

  const credMenu = (c: DnsCredential): MenuItem[] => [
    { label: 'Test…', onSelect: () => setTesting(c), testId: 'tls-dns-test' },
    { divider: true },
    {
      label: 'Delete',
      danger: true,
      onSelect: () => {
        setDeleteError(null)
        setDeleteCred(c)
      },
      testId: 'tls-dns-delete',
    },
  ]

  return (
    <>
      <section className="settings-section" data-testid="tls-certs-section">
        <div className="settings-section-head">
          <h3>Certificates</h3>
          <div className="acct-row-actions">
            <button
              type="button"
              className="btn-primary btn-sm"
              onClick={() => setAddingCert(true)}
              data-testid="tls-add-cert"
            >
              + Add certificate
            </button>
          </div>
        </div>
        <p className="form-hint" data-testid="tls-acme-help">
          Let&apos;s Encrypt certificates are issued with the DNS-01 challenge: Peckboard publishes
          a temporary TXT record through your DNS provider, so no port forwarding or public HTTP
          access is needed, and wildcards (<code>*.example.com</code>) are supported. Each
          connection gets the certificate matching the hostname it asked for (SNI); anything no
          certificate covers falls back to the uploaded certificate, then the self-signed one.
          Certificates renew automatically 30 days before they expire.
        </p>
        {actionError && (
          <p className="form-error" role="alert" data-testid="tls-certs-error">
            {actionError}
          </p>
        )}
        <List<TlsCertificate>
          items={certs}
          getKey={(c) => c.id}
          bodyClassName="list-view-rows"
          onActivate={() => {}}
          getMenuItems={certMenu}
          renderItem={(c) => (
            <>
              <span className="list-view-name" data-testid="tls-cert-row" data-cert-id={c.id}>
                {c.domains.join(', ')}
              </span>
              <span className="list-view-meta">
                <span className="list-view-tag">
                  {c.source === 'acme' ? "Let's Encrypt" : 'Uploaded'}
                </span>
                {c.staging && (
                  <span className="list-view-tag" data-testid="tls-cert-staging-tag">
                    staging
                  </span>
                )}
                <span
                  className={`status-badge ${STATUS_CLASS[c.status]}`}
                  data-testid="tls-cert-status"
                >
                  {c.status}
                </span>
                {c.status === 'error' && c.error && (
                  <span className="form-error" data-testid="tls-cert-error-text">
                    {c.error}
                  </span>
                )}
                {c.not_after && <span>Expires {formatExpiry(c.not_after)}</span>}
              </span>
            </>
          )}
          emptyState={
            <div className="list-view-empty" data-testid="tls-certs-empty">
              No managed certificates yet.
            </div>
          }
        />
      </section>

      <section className="settings-section" data-testid="tls-dns-section">
        <div className="settings-section-head">
          <h3>DNS Providers</h3>
          <div className="acct-row-actions">
            <button
              type="button"
              className="btn-secondary btn-sm"
              onClick={() => setAddingDns(true)}
              disabled={providers.length === 0}
              data-testid="tls-add-dns"
            >
              + Add DNS provider
            </button>
          </div>
        </div>
        <p className="form-hint">
          Credentials used to answer DNS-01 challenges. Secret values are stored on the server and
          only shown masked.
        </p>
        <List<DnsCredential>
          items={creds}
          getKey={(c) => c.id}
          bodyClassName="list-view-rows"
          onActivate={(c) => setTesting(c)}
          getMenuItems={credMenu}
          renderItem={(c) => (
            <>
              <span className="list-view-name" data-testid="tls-dns-row" data-cred-id={c.id}>
                {c.label}
              </span>
              <span className="list-view-meta">
                <span className="list-view-tag">{providerName(c.provider)}</span>
                {Object.entries(c.fields).map(([k, v]) => (
                  <span key={k}>
                    {k}: {v}
                  </span>
                ))}
              </span>
            </>
          )}
          emptyState={
            <div className="list-view-empty" data-testid="tls-dns-empty">
              No DNS providers saved yet.
            </div>
          }
        />
      </section>

      {addingCert && (
        <AddCertificateModal
          credentials={creds}
          providers={providers}
          onClose={() => setAddingCert(false)}
          onCreated={() => void reload()}
        />
      )}
      {addingDns && (
        <AddDnsCredentialModal
          providers={providers}
          onClose={() => setAddingDns(false)}
          onCreated={() => void reload()}
        />
      )}
      {testing && <TestDnsCredentialModal cred={testing} onClose={() => setTesting(null)} />}
      {deleteCert && (
        <ConfirmDialog
          testId="tls-cert-delete-confirm"
          danger
          title="Delete certificate?"
          message={`Delete the certificate for ${deleteCert.domains.join(', ')}? Its key is removed and connections for these hostnames fall back to the next matching certificate.`}
          confirmLabel="Delete"
          busy={deleteBusy}
          busyLabel="Deleting…"
          error={deleteError}
          onConfirm={() =>
            void runDelete(`/api/settings/tls/certificates/${deleteCert.id}`, () =>
              setDeleteCert(null),
            )
          }
          onCancel={() => setDeleteCert(null)}
        />
      )}
      {deleteCred && (
        <ConfirmDialog
          testId="tls-dns-delete-confirm"
          danger
          title="Delete DNS provider?"
          message={`Delete "${deleteCred.label}"? Certificates still using it must be deleted first.`}
          confirmLabel="Delete"
          busy={deleteBusy}
          busyLabel="Deleting…"
          error={deleteError}
          onConfirm={() =>
            void runDelete(`/api/settings/tls/dns-credentials/${deleteCred.id}`, () =>
              setDeleteCred(null),
            )
          }
          onCancel={() => setDeleteCred(null)}
        />
      )}
    </>
  )
}
