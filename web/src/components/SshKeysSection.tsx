import { useEffect, useState } from 'react'
import { useAuthStore } from '../store/auth'
import { useSshKeysStore } from '../store/sshKeys'
import type { SshKey } from '../types/api'
import { copyText } from '../utils/clipboard'
import ConfirmDialog from './ConfirmDialog'
import type { MenuItem } from './Dropdown'
import FieldError from './FieldError'
import List from './List'
import Modal from './Modal'
import SecretInput from './SecretInput'

/** The key types the server's `generate_keypair` accepts — today that is
 *  ed25519 only (`service::ssh_keys::generate_keypair` rejects anything
 *  else with a 400). A fixed option set, so it renders as a <select>
 *  rather than a text field; listing a type the server would refuse would
 *  just be a dropdown entry that always errors. RSA and ECDSA keys are
 *  still supported through Import, which parses whatever `russh` reads. */
const KEY_TYPES: { value: string; label: string }[] = [
  { value: 'ed25519', label: 'Ed25519 (recommended)' },
]

/** Fingerprints are long (`SHA256:` + 43 base64 chars); the row shows a
 *  recognisable head and keeps the full value in the title. */
function truncateFingerprint(fp: string): string {
  return fp.length <= 26 ? fp : `${fp.slice(0, 26)}…`
}

function formatDate(iso: string): string {
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleDateString()
}

/** Route a server error to the field it's about, so the message lands
 *  under the input the user has to change (FieldError) instead of in a
 *  form-wide banner that names fields they filled in correctly. */
function fieldFor(message: string): 'name' | 'private_key' | null {
  if (message.includes('private key')) return 'private_key'
  if (message.includes('name')) return 'name'
  return null
}

/**
 * Settings section for the SSH key vault: import an existing private key,
 * generate a new one, copy the public half, rename, delete.
 *
 * Private key material only ever travels client → server: the list and
 * every mutation response carry metadata (`fingerprint`, `public_key`)
 * only, so there is nothing here that could leak a secret back out.
 * Mutations are admin-only server-side; non-admins get a read-only view
 * with the reason stated rather than a silent 403.
 */
export default function SshKeysSection() {
  const isAdmin = useAuthStore((s) => s.user?.role === 'admin')
  const keys = useSshKeysStore((s) => s.keys)
  const loaded = useSshKeysStore((s) => s.loaded)
  const error = useSshKeysStore((s) => s.error)
  const setError = useSshKeysStore((s) => s.setError)
  const fetchKeys = useSshKeysStore((s) => s.fetchKeys)
  const deleteKey = useSshKeysStore((s) => s.deleteKey)
  const fetchPublicKey = useSshKeysStore((s) => s.fetchPublicKey)

  const [showImport, setShowImport] = useState(false)
  const [showGenerate, setShowGenerate] = useState(false)
  const [renaming, setRenaming] = useState<SshKey | null>(null)
  const [confirmDelete, setConfirmDelete] = useState<SshKey | null>(null)
  const [deleteError, setDeleteError] = useState<string | null>(null)
  const [deleting, setDeleting] = useState(false)
  const [copied, setCopied] = useState<string | null>(null)

  useEffect(() => {
    void fetchKeys()
  }, [fetchKeys])

  const copyPublicKey = async (key: SshKey) => {
    setError(null)
    try {
      // Re-read from the server rather than trusting the cached row: the
      // public key is what gets pasted into `authorized_keys`, so it has to
      // be the one the vault holds right now.
      const pub = await fetchPublicKey(key.id)
      const ok = await copyText(pub)
      if (!ok) throw new Error('Clipboard unavailable')
      setCopied(key.id)
      window.setTimeout(() => setCopied((c) => (c === key.id ? null : c)), 2000)
    } catch (e) {
      setError(`Could not copy the public key: ${(e as Error).message}`)
    }
  }

  const removeKey = async (key: SshKey) => {
    setDeleting(true)
    setDeleteError(null)
    try {
      await deleteKey(key.id)
      setConfirmDelete(null)
    } catch (e) {
      setDeleteError((e as Error).message)
    } finally {
      setDeleting(false)
    }
  }

  const buildMenu = (key: SshKey): MenuItem[] => [
    { label: 'Copy public key', onSelect: () => void copyPublicKey(key) },
    { label: 'Rename', hidden: !isAdmin, onSelect: () => setRenaming(key) },
    { divider: true, hidden: !isAdmin },
    {
      label: 'Delete',
      danger: true,
      hidden: !isAdmin,
      onSelect: () => {
        setDeleteError(null)
        setConfirmDelete(key)
      },
    },
  ]

  return (
    <section className="settings-section" data-testid="ssh-keys-section">
      <h3>SSH Keys</h3>
      <p className="form-hint">
        Private keys for reaching your servers, encrypted at rest and never returned by the API —
        only the public half leaves PeckBoard. Copy a key&rsquo;s public half into a server&rsquo;s{' '}
        <code>authorized_keys</code> to let PeckBoard connect as that key.
      </p>

      {error && <p className="settings-error">{error}</p>}

      {!loaded ? (
        <p className="settings-loading">Loading SSH keys...</p>
      ) : (
        <List<SshKey>
          items={keys}
          getKey={(k) => k.id}
          bodyClassName="list-view-rows"
          onActivate={(k) => void copyPublicKey(k)}
          getMenuItems={buildMenu}
          emptyState={
            <p className="settings-loading">
              No SSH keys yet. {isAdmin ? 'Import or generate one below.' : ''}
            </p>
          }
          renderItem={(k) => (
            <>
              <span className="list-view-name" data-testid={`ssh-key-name-${k.name}`}>
                {k.name}
              </span>
              <span className="list-view-meta">
                <span className="status-badge">{k.key_type}</span>
                <span
                  className="list-view-time"
                  title={k.fingerprint}
                  data-testid={`ssh-key-fingerprint-${k.name}`}
                >
                  {truncateFingerprint(k.fingerprint)}
                </span>
                <span className="list-view-time">{formatDate(k.created_at)}</span>
                {copied === k.id && <span className="list-view-time">Public key copied</span>}
              </span>
            </>
          )}
        />
      )}

      <div className="form-actions">
        {!isAdmin && (
          <span className="form-actions-reason" data-testid="ssh-keys-disabled-reason">
            Only an admin can import, generate, rename or delete SSH keys.
          </span>
        )}
        <button
          type="button"
          className="btn-secondary"
          disabled={!isAdmin}
          onClick={() => setShowImport(true)}
          data-testid="ssh-key-import-btn"
        >
          Import key
        </button>
        <button
          type="button"
          className="btn-primary"
          disabled={!isAdmin}
          onClick={() => setShowGenerate(true)}
          data-testid="ssh-key-generate-btn"
        >
          Generate key
        </button>
      </div>

      {showImport && <ImportKeyModal onClose={() => setShowImport(false)} />}
      {showGenerate && <GenerateKeyModal onClose={() => setShowGenerate(false)} />}
      {renaming && <RenameKeyModal keyRow={renaming} onClose={() => setRenaming(null)} />}
      {confirmDelete && (
        <ConfirmDialog
          testId="ssh-key-delete-confirm"
          danger
          title={`Delete ${confirmDelete.name}?`}
          message={`The private key is destroyed for good — it can't be exported first, and nothing can recover it. Any host or plugin configured to connect with "${confirmDelete.name}" stops working until it is pointed at another key, and the public key stays in every server's authorized_keys until you remove it there.`}
          confirmLabel="Delete key"
          error={deleteError}
          busy={deleting}
          busyLabel="Deleting…"
          onConfirm={() => void removeKey(confirmDelete)}
          onCancel={() => {
            setConfirmDelete(null)
            setDeleteError(null)
          }}
        />
      )}
    </section>
  )
}

/** Import a pasted private key. The server does the real parsing — a
 *  malformed or wrongly-passphrased key comes back as a 400 and lands under
 *  the field it's about. */
function ImportKeyModal({ onClose }: { onClose: () => void }) {
  const importKey = useSshKeysStore((s) => s.importKey)
  const [name, setName] = useState('')
  const [privateKey, setPrivateKey] = useState('')
  const [passphrase, setPassphrase] = useState('')
  const [nameError, setNameError] = useState<string | null>(null)
  const [keyError, setKeyError] = useState<string | null>(null)
  const [formError, setFormError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)

  const disabledReason =
    name.trim().length === 0
      ? 'Give the key a name.'
      : privateKey.trim().length === 0
        ? 'Paste the private key.'
        : null

  const submit = async () => {
    if (saving || disabledReason) return
    setSaving(true)
    setNameError(null)
    setKeyError(null)
    setFormError(null)
    try {
      await importKey({
        name: name.trim(),
        private_key: privateKey,
        passphrase: passphrase.length > 0 ? passphrase : undefined,
      })
      onClose()
    } catch (e) {
      const message = (e as Error).message
      const field = fieldFor(message)
      if (field === 'private_key') setKeyError(message)
      else if (field === 'name') setNameError(message)
      else setFormError(message)
    } finally {
      setSaving(false)
    }
  }

  return (
    <Modal onClose={onClose} maxWidth={560} data-testid="ssh-key-import-modal">
      <form
        onSubmit={(e) => {
          e.preventDefault()
          void submit()
        }}
      >
        <h3>Import SSH key</h3>
        <p className="form-hint">
          The private key is encrypted with the host&rsquo;s vault key as soon as it arrives and is
          never sent back to a browser — keep your own copy if you need one elsewhere.
        </p>
        <div className="form-field">
          <label className="form-label" htmlFor="ssh-key-import-name">
            Name
          </label>
          <input
            id="ssh-key-import-name"
            className="form-input"
            value={name}
            autoComplete="off"
            placeholder="prod-deploy"
            onChange={(e) => setName(e.target.value)}
            data-testid="ssh-key-import-name-input"
          />
          <FieldError message={nameError ?? undefined} testId="ssh-key-import-name-error" />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="ssh-key-import-private">
            Private key
          </label>
          <SecretInput
            id="ssh-key-import-private"
            className="form-input"
            multiline
            rows={8}
            label="private key"
            value={privateKey}
            onChange={setPrivateKey}
            placeholder="-----BEGIN OPENSSH PRIVATE KEY-----"
            testId="ssh-key-import-private-input"
            revealTestId="ssh-key-import-private-reveal"
          />
          <FieldError message={keyError ?? undefined} testId="ssh-key-import-private-error" />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="ssh-key-import-passphrase">
            Passphrase (optional)
          </label>
          <SecretInput
            id="ssh-key-import-passphrase"
            className="form-input"
            label="passphrase"
            value={passphrase}
            onChange={setPassphrase}
            testId="ssh-key-import-passphrase-input"
            revealTestId="ssh-key-import-passphrase-reveal"
          />
          <p className="form-hint">Only if the key is passphrase-protected.</p>
        </div>
        {formError && <p className="form-error">{formError}</p>}
        <div className="form-actions">
          {!saving && disabledReason && (
            <span className="form-actions-reason" data-testid="ssh-key-import-disabled-reason">
              {disabledReason}
            </span>
          )}
          <button type="button" className="btn-secondary" onClick={onClose}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={saving || !!disabledReason}
            data-testid="ssh-key-import-submit"
          >
            {saving ? 'Importing…' : 'Import key'}
          </button>
        </div>
      </form>
    </Modal>
  )
}

/** Generate a fresh keypair server-side. */
function GenerateKeyModal({ onClose }: { onClose: () => void }) {
  const generateKey = useSshKeysStore((s) => s.generateKey)
  const [name, setName] = useState('')
  const [keyType, setKeyType] = useState(KEY_TYPES[0].value)
  const [nameError, setNameError] = useState<string | null>(null)
  const [formError, setFormError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)

  const disabledReason = name.trim().length === 0 ? 'Give the key a name.' : null

  const submit = async () => {
    if (saving || disabledReason) return
    setSaving(true)
    setNameError(null)
    setFormError(null)
    try {
      await generateKey({ name: name.trim(), key_type: keyType })
      onClose()
    } catch (e) {
      const message = (e as Error).message
      if (fieldFor(message) === 'name') setNameError(message)
      else setFormError(message)
    } finally {
      setSaving(false)
    }
  }

  return (
    <Modal onClose={onClose} maxWidth={480} data-testid="ssh-key-generate-modal">
      <form
        onSubmit={(e) => {
          e.preventDefault()
          void submit()
        }}
      >
        <h3>Generate SSH key</h3>
        <p className="form-hint">
          The private half is generated on the server, encrypted at rest, and{' '}
          <strong>can never be exported or displayed</strong> — not even by an admin. Only the
          public key is shown afterwards, for pasting into a server&rsquo;s{' '}
          <code>authorized_keys</code>. If you need the private key on another machine, import one
          you already hold instead.
        </p>
        <div className="form-field">
          <label className="form-label" htmlFor="ssh-key-generate-name">
            Name
          </label>
          <input
            id="ssh-key-generate-name"
            className="form-input"
            value={name}
            autoComplete="off"
            placeholder="prod-deploy"
            onChange={(e) => setName(e.target.value)}
            data-testid="ssh-key-generate-name-input"
          />
          <FieldError message={nameError ?? undefined} testId="ssh-key-generate-name-error" />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="ssh-key-generate-type">
            Key type
          </label>
          <select
            id="ssh-key-generate-type"
            className="form-input"
            value={keyType}
            onChange={(e) => setKeyType(e.target.value)}
            data-testid="ssh-key-generate-type-select"
          >
            {KEY_TYPES.map((t) => (
              <option key={t.value} value={t.value}>
                {t.label}
              </option>
            ))}
          </select>
          <p className="form-hint">
            Ed25519 is the only type PeckBoard generates. An RSA or ECDSA key you already hold can
            be brought in with Import.
          </p>
        </div>
        {formError && <p className="form-error">{formError}</p>}
        <div className="form-actions">
          {!saving && disabledReason && (
            <span className="form-actions-reason" data-testid="ssh-key-generate-disabled-reason">
              {disabledReason}
            </span>
          )}
          <button type="button" className="btn-secondary" onClick={onClose}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={saving || !!disabledReason}
            data-testid="ssh-key-generate-submit"
          >
            {saving ? 'Generating…' : 'Generate key'}
          </button>
        </div>
      </form>
    </Modal>
  )
}

/** Rename an existing key. The name is how hosts and plugins refer to it. */
function RenameKeyModal({ keyRow, onClose }: { keyRow: SshKey; onClose: () => void }) {
  const renameKey = useSshKeysStore((s) => s.renameKey)
  const [name, setName] = useState(keyRow.name)
  const [nameError, setNameError] = useState<string | null>(null)
  const [formError, setFormError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)

  const disabledReason =
    name.trim().length === 0
      ? 'Give the key a name.'
      : name.trim() === keyRow.name
        ? 'Change the name first.'
        : null

  const submit = async () => {
    if (saving || disabledReason) return
    setSaving(true)
    setNameError(null)
    setFormError(null)
    try {
      await renameKey(keyRow.id, name.trim())
      onClose()
    } catch (e) {
      const message = (e as Error).message
      if (fieldFor(message) === 'name') setNameError(message)
      else setFormError(message)
    } finally {
      setSaving(false)
    }
  }

  return (
    <Modal onClose={onClose} maxWidth={420} data-testid="ssh-key-rename-modal">
      <form
        onSubmit={(e) => {
          e.preventDefault()
          void submit()
        }}
      >
        <h3>Rename {keyRow.name}</h3>
        <div className="form-field">
          <label className="form-label" htmlFor="ssh-key-rename-name">
            Name
          </label>
          <input
            id="ssh-key-rename-name"
            className="form-input"
            value={name}
            autoComplete="off"
            onChange={(e) => setName(e.target.value)}
            data-testid="ssh-key-rename-name-input"
          />
          <FieldError message={nameError ?? undefined} testId="ssh-key-rename-name-error" />
        </div>
        {formError && <p className="form-error">{formError}</p>}
        <div className="form-actions">
          {!saving && disabledReason && (
            <span className="form-actions-reason" data-testid="ssh-key-rename-disabled-reason">
              {disabledReason}
            </span>
          )}
          <button type="button" className="btn-secondary" onClick={onClose}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={saving || !!disabledReason}
            data-testid="ssh-key-rename-submit"
          >
            {saving ? 'Saving…' : 'Rename'}
          </button>
        </div>
      </form>
    </Modal>
  )
}
