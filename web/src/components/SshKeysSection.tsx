import { useEffect, useRef, useState, type FormEvent } from 'react'
import { copyText } from '../lib/clipboard'
import { readFileAsText } from '../lib/readFileAsText'
import { useAuthStore } from '../store/auth'
import { useSshKeysStore } from '../store/sshKeys'
import type { SshKey } from '../types/api'
import ConfirmDialog from './ConfirmDialog'
import FieldError from './FieldError'
import List from './List'
import Modal from './Modal'
import RenameModal from './RenameModal'
import SecretInput from './SecretInput'
import type { MenuItem } from './Dropdown'

/** Key types the server can generate. Generation is ed25519-only today
 *  (`src/service/ssh_keys.rs`), so this is a fixed option set, not free text. */
const GENERATE_KEY_TYPES = [{ value: 'ed25519', label: 'ed25519 (recommended)' }]

const ADMIN_ONLY_REASON = 'Only admins can add, rename or delete SSH keys.'

/** Fingerprints are ~50 chars of base64; the head is enough to recognise a
 *  key at a glance, and the full value is in the title + the public-key
 *  dialog. */
function shortFingerprint(fp: string): string {
  return fp.length > 28 ? `${fp.slice(0, 28)}…` : fp
}

function formatCreated(iso: string): string {
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleDateString()
}

/** Read-only view of one key's public half — the line a user pastes into a
 *  server's `authorized_keys`. Pulled from `/api/ssh-keys/{id}/public` so
 *  what's shown is what the server would hand out. */
function PublicKeyModal({ sshKey, onClose }: { sshKey: SshKey; onClose: () => void }) {
  const fetchPublicKey = useSshKeysStore((s) => s.fetchPublicKey)
  const [text, setText] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [copied, setCopied] = useState(false)

  useEffect(() => {
    let cancelled = false
    fetchPublicKey(sshKey.id)
      .then((pk) => {
        if (!cancelled) setText(pk)
      })
      .catch((e: unknown) => {
        if (!cancelled) setError(e instanceof Error ? e.message : 'Failed to read the public key')
      })
    return () => {
      cancelled = true
    }
  }, [fetchPublicKey, sshKey.id])

  return (
    <Modal onClose={onClose} maxWidth={640} data-testid="ssh-key-public-modal">
      <h2>Public key — {sshKey.name}</h2>
      <p className="form-hint">
        Paste this line into the target server&rsquo;s <code>~/.ssh/authorized_keys</code>. The
        matching private key stays sealed in Peckboard.
      </p>
      <div className="form-field">
        <label className="form-label" htmlFor="ssh-key-public-text">
          {sshKey.key_type} · {sshKey.fingerprint}
        </label>
        <textarea
          id="ssh-key-public-text"
          className="form-input"
          rows={3}
          readOnly
          value={text ?? (error ? '' : 'Loading…')}
          data-testid="ssh-key-public-text"
        />
        <FieldError message={error ?? undefined} testId="ssh-key-public-error" />
      </div>
      <div className="form-actions">
        <button type="button" className="btn-secondary" onClick={onClose}>
          Close
        </button>
        <button
          type="button"
          className="btn-primary"
          disabled={!text}
          onClick={() => {
            if (!text) return
            void copyText(text).then((ok) => {
              setCopied(ok)
              if (ok) setTimeout(() => setCopied(false), 2000)
            })
          }}
          data-testid="ssh-key-public-copy"
        >
          {copied ? 'Copied' : 'Copy public key'}
        </button>
      </div>
    </Modal>
  )
}

/** Import a private key the user already has. The server does the real
 *  parsing (and the passphrase check) — its rejection is surfaced on the
 *  field that caused it rather than as a raw body. */
function ImportKeyModal({ onClose }: { onClose: () => void }) {
  const importKey = useSshKeysStore((s) => s.importKey)
  const [name, setName] = useState('')
  const [privateKey, setPrivateKey] = useState('')
  const [passphrase, setPassphrase] = useState('')
  const [nameError, setNameError] = useState('')
  const [keyError, setKeyError] = useState('')
  const [formError, setFormError] = useState('')
  const [busy, setBusy] = useState(false)
  const keyFileRef = useRef<HTMLInputElement>(null)

  const disabledReason = !name.trim()
    ? 'Enter a name for the key.'
    : !privateKey.trim()
      ? 'Paste the private key to import.'
      : null

  const pickKeyFile = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0]
    e.target.value = ''
    if (!file) return
    setKeyError('')
    setFormError('')
    try {
      setPrivateKey(await readFileAsText(file))
    } catch {
      setKeyError(`Could not read ${file.name}.`)
    }
  }

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    if (disabledReason || busy) return
    setNameError('')
    setKeyError('')
    setFormError('')
    setBusy(true)
    try {
      await importKey({
        name: name.trim(),
        private_key: privateKey,
        passphrase: passphrase || undefined,
      })
      onClose()
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Failed to import key'
      // Only the server's own field rejections belong on a field. Anything
      // else (403 from a non-admin, a vault failure) is form-level — putting
      // it under "Private key" would tell the user their key is malformed
      // when it isn't.
      if (/private key|passphrase/i.test(message)) setKeyError(message)
      else if (/name/i.test(message)) setNameError(message)
      else setFormError(message)
      setBusy(false)
    }
  }

  return (
    <Modal onClose={onClose} maxWidth={560} data-testid="ssh-key-import-modal">
      <h2>Import SSH Key</h2>
      <p className="form-hint">
        The key is checked before anything is stored, then sealed with the host&rsquo;s vault key.
        It is never returned to a browser again.
      </p>
      <form onSubmit={handleSubmit}>
        <div className="form-field">
          <label className="form-label" htmlFor="ssh-import-name">
            Name
          </label>
          <input
            id="ssh-import-name"
            className="form-input"
            type="text"
            value={name}
            autoComplete="off"
            placeholder="e.g. prod-deploy"
            onChange={(e) => {
              setName(e.target.value)
              setNameError('')
            }}
            autoFocus
            data-testid="ssh-import-name"
          />
          <FieldError message={nameError} testId="ssh-import-name-error" />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="ssh-import-private">
            Private key (PEM)
          </label>
          {/* A PEM is multi-line: a single-line <input> drops the newlines on
              paste and the server then rejects every real key. Same shape as
              the TLS upload fields. */}
          <textarea
            id="ssh-import-private"
            className="form-input"
            rows={6}
            value={privateKey}
            spellCheck={false}
            placeholder="-----BEGIN OPENSSH PRIVATE KEY-----"
            onChange={(e) => {
              setPrivateKey(e.target.value)
              setKeyError('')
            }}
            data-testid="ssh-import-private"
          />
          <input
            ref={keyFileRef}
            type="file"
            accept=".pem,.key,.txt"
            hidden
            onChange={(e) => void pickKeyFile(e)}
            data-testid="ssh-import-private-file"
          />
          <button
            type="button"
            className="btn-secondary"
            onClick={() => keyFileRef.current?.click()}
            disabled={busy}
            data-testid="ssh-import-private-choose"
          >
            Choose key file…
          </button>
          <FieldError message={keyError} testId="ssh-import-private-error" />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="ssh-import-passphrase">
            Passphrase (optional)
          </label>
          <SecretInput
            id="ssh-import-passphrase"
            className="form-input"
            label="passphrase"
            value={passphrase}
            onChange={setPassphrase}
            testId="ssh-import-passphrase"
            revealTestId="ssh-import-passphrase-reveal"
          />
          <span className="form-hint">Only needed if the key is passphrase-protected.</span>
        </div>
        {formError && (
          <p className="form-error" data-testid="ssh-import-form-error">
            {formError}
          </p>
        )}
        <div className="form-actions">
          {!busy && disabledReason && (
            <span className="form-actions-reason" data-testid="ssh-import-disabled-reason">
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
            data-testid="ssh-import-submit"
          >
            {busy ? 'Importing…' : 'Import key'}
          </button>
        </div>
      </form>
    </Modal>
  )
}

/** Generate a fresh keypair server-side. The private half is sealed on
 *  creation and there is no export path — that has to be said up front. */
function GenerateKeyModal({ onClose }: { onClose: () => void }) {
  const generateKey = useSshKeysStore((s) => s.generateKey)
  const [name, setName] = useState('')
  const [keyType, setKeyType] = useState(GENERATE_KEY_TYPES[0].value)
  const [nameError, setNameError] = useState('')
  const [formError, setFormError] = useState('')
  const [busy, setBusy] = useState(false)

  const disabledReason = !name.trim() ? 'Enter a name for the key.' : null

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    if (disabledReason || busy) return
    setNameError('')
    setFormError('')
    setBusy(true)
    try {
      await generateKey({ name: name.trim(), key_type: keyType })
      onClose()
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Failed to generate key'
      if (message.includes('name')) setNameError(message)
      else setFormError(message)
      setBusy(false)
    }
  }

  return (
    <Modal onClose={onClose} maxWidth={520} data-testid="ssh-key-generate-modal">
      <h2>Generate SSH Key</h2>
      <p className="form-hint">
        Peckboard creates the keypair and seals the private half with the host&rsquo;s vault key.
        The private key cannot be exported or shown again — copy the <em>public</em> key to the
        servers this key should reach.
      </p>
      <form onSubmit={handleSubmit}>
        <div className="form-field">
          <label className="form-label" htmlFor="ssh-generate-name">
            Name
          </label>
          <input
            id="ssh-generate-name"
            className="form-input"
            type="text"
            value={name}
            autoComplete="off"
            placeholder="e.g. fleet-admin"
            onChange={(e) => {
              setName(e.target.value)
              setNameError('')
            }}
            autoFocus
            data-testid="ssh-generate-name"
          />
          <FieldError message={nameError} testId="ssh-generate-name-error" />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="ssh-generate-type">
            Key type
          </label>
          <select
            id="ssh-generate-type"
            className="form-input"
            value={keyType}
            onChange={(e) => setKeyType(e.target.value)}
            data-testid="ssh-generate-type"
          >
            {GENERATE_KEY_TYPES.map((t) => (
              <option key={t.value} value={t.value}>
                {t.label}
              </option>
            ))}
          </select>
        </div>
        {formError && <p className="form-error">{formError}</p>}
        <div className="form-actions">
          {!busy && disabledReason && (
            <span className="form-actions-reason" data-testid="ssh-generate-disabled-reason">
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
            data-testid="ssh-generate-submit"
          >
            {busy ? 'Generating…' : 'Generate key'}
          </button>
        </div>
      </form>
    </Modal>
  )
}

/**
 * Settings section for the SSH key vault: the keys plugins and SSH hosts
 * authenticate with. Private key material is sealed server-side and never
 * reaches the browser — every row shows metadata plus the public key, which
 * is the half users actually need to hand out.
 *
 * Reads are open to any authenticated user; every mutation is admin-only on
 * the API, so non-admins get the controls disabled with the reason stated
 * rather than a silent 403.
 */
export default function SshKeysSection() {
  const isAdmin = useAuthStore((s) => s.user?.role === 'admin')
  const keys = useSshKeysStore((s) => s.keys)
  const loaded = useSshKeysStore((s) => s.loaded)
  const error = useSshKeysStore((s) => s.error)
  const fetchKeys = useSshKeysStore((s) => s.fetchKeys)
  const renameKey = useSshKeysStore((s) => s.renameKey)
  const deleteKey = useSshKeysStore((s) => s.deleteKey)
  const setError = useSshKeysStore((s) => s.setError)

  const [showImport, setShowImport] = useState(false)
  const [showGenerate, setShowGenerate] = useState(false)
  const [viewing, setViewing] = useState<SshKey | null>(null)
  const [renaming, setRenaming] = useState<SshKey | null>(null)
  const [confirmDelete, setConfirmDelete] = useState<SshKey | null>(null)
  const [deleteError, setDeleteError] = useState<string | null>(null)
  const [deleting, setDeleting] = useState(false)
  const [copiedId, setCopiedId] = useState<string | null>(null)

  useEffect(() => {
    void fetchKeys()
  }, [fetchKeys])

  const buildMenu = (k: SshKey): MenuItem[] => [
    {
      label: copiedId === k.id ? 'Copied' : 'Copy public key',
      // Revert the label, or the menu reads "Copied" for the rest of the
      // session and stops telling the user what the item does.
      onSelect: () =>
        void copyText(k.public_key).then((ok) => {
          setCopiedId(ok ? k.id : null)
          if (ok) setTimeout(() => setCopiedId((id) => (id === k.id ? null : id)), 2000)
        }),
      testId: `ssh-key-copy-${k.id}`,
    },
    { label: 'Show public key', onSelect: () => setViewing(k) },
    { divider: true },
    {
      label: 'Rename',
      onSelect: () => setRenaming(k),
      disabled: !isAdmin,
      hint: isAdmin ? undefined : 'Admins only',
    },
    {
      label: 'Delete',
      danger: true,
      onSelect: () => {
        setDeleteError(null)
        setConfirmDelete(k)
      },
      disabled: !isAdmin,
      hint: isAdmin ? undefined : 'Admins only',
    },
  ]

  return (
    <section className="settings-section" data-testid="ssh-keys-section">
      <div className="settings-section-head">
        <h3>SSH Keys</h3>
        <div className="acct-row-actions">
          <button
            type="button"
            className="btn-secondary btn-sm"
            onClick={() => setShowImport(true)}
            disabled={!isAdmin}
            data-testid="ssh-key-import"
          >
            Import key
          </button>
          <button
            type="button"
            className="btn-primary btn-sm"
            onClick={() => setShowGenerate(true)}
            disabled={!isAdmin}
            data-testid="ssh-key-generate"
          >
            + Generate key
          </button>
        </div>
      </div>
      <p className="form-hint">
        Keys SSH hosts and plugins authenticate with. The private half is encrypted with the
        host&rsquo;s vault key and never leaves the server — copy the <em>public</em> key into a
        target server&rsquo;s <code>authorized_keys</code>.
      </p>
      {!isAdmin && (
        <span className="form-actions-reason" data-testid="ssh-keys-readonly-reason">
          {ADMIN_ONLY_REASON} You can still read the list and copy public keys.
        </span>
      )}

      {error && (
        <div className="form-error" data-testid="ssh-keys-error">
          <span>{error}</span>{' '}
          <button
            type="button"
            className="btn-secondary"
            onClick={() => void fetchKeys()}
            data-testid="ssh-keys-retry"
          >
            Retry
          </button>
        </div>
      )}

      <List<SshKey>
        items={keys}
        getKey={(k) => k.id}
        bodyClassName="list-view-rows"
        onActivate={(k) => setViewing(k)}
        getMenuItems={buildMenu}
        renderItem={(k) => (
          <>
            <span className="list-view-name" data-testid={`ssh-key-row-${k.name}`}>
              {k.name}
            </span>
            <span className="list-view-meta">
              <span className="list-view-tag">{k.key_type}</span>
              {k.has_passphrase && <span className="list-view-tag">passphrase</span>}
              <span title={k.fingerprint} data-testid={`ssh-key-fingerprint-${k.name}`}>
                {shortFingerprint(k.fingerprint)}
              </span>
              <span>{formatCreated(k.created_at)}</span>
            </span>
          </>
        )}
        emptyState={
          <div className="list-view-empty" data-testid="ssh-keys-empty">
            {error
              ? 'The key list could not be loaded.'
              : loaded
                ? 'No SSH keys yet — generate one or import an existing key.'
                : 'Loading…'}
          </div>
        }
      />

      {showImport && <ImportKeyModal onClose={() => setShowImport(false)} />}
      {showGenerate && <GenerateKeyModal onClose={() => setShowGenerate(false)} />}
      {viewing && <PublicKeyModal sshKey={viewing} onClose={() => setViewing(null)} />}
      {renaming && (
        <RenameModal
          title="Rename SSH key"
          label="Key name"
          initialValue={renaming.name}
          onSubmit={(name) => renameKey(renaming.id, name)}
          onClose={() => setRenaming(null)}
        />
      )}
      {confirmDelete && (
        <ConfirmDialog
          title="Delete SSH key"
          message={`Delete "${confirmDelete.name}"? Any SSH host configured to use this key will stop connecting until it is pointed at another key. The private key is destroyed and cannot be recovered.`}
          confirmLabel="Delete"
          danger
          busy={deleting}
          busyLabel="Deleting…"
          error={deleteError}
          testId="ssh-key-delete-confirm"
          onConfirm={() => {
            const target = confirmDelete
            setDeleteError(null)
            setDeleting(true)
            setError(null)
            void deleteKey(target.id)
              .then(() => setConfirmDelete(null))
              .catch((e: unknown) =>
                setDeleteError(e instanceof Error ? e.message : 'Failed to delete key'),
              )
              .finally(() => setDeleting(false))
          }}
          onCancel={() => setConfirmDelete(null)}
        />
      )}
    </section>
  )
}
