import { useCallback, useEffect, useState, type FormEvent } from 'react'
import { copyText } from '../lib/clipboard'
import { authedFetch } from '../store/auth'
import ConfirmDialog from './ConfirmDialog'
import FieldError from './FieldError'
import List from './List'
import Modal from './Modal'
import RenameModal from './RenameModal'
import type { MenuItem } from './Dropdown'

interface DeviceStatus {
  state: 'offline' | 'waiting' | 'connected' | 'error'
  peer: string | null
  rtt_ms: number | null
  error: string | null
}

interface RemoteDevice {
  id: string
  name: string
  created_at: string
  last_connected_at: string | null
  status: DeviceStatus
}

interface Overview {
  enabled: boolean
  relay_host: string
  devices: RemoteDevice[]
}

interface Pairing {
  device: RemoteDevice
  pairing_link: string
  qr_svg: string
}

const POLL_MS = 5000

const STATE_LABEL: Record<DeviceStatus['state'], string> = {
  offline: 'offline',
  waiting: 'waiting for device',
  connected: 'connected',
  error: 'error — retrying',
}

async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await authedFetch(path, {
    ...init,
    headers: init?.body ? { 'Content-Type': 'application/json' } : undefined,
  })
  if (!res.ok) {
    const body = (await res.json().catch(() => null)) as { error?: string } | null
    throw new Error(body?.error || `Request failed (${res.status})`)
  }
  return res.status === 204 ? (undefined as T) : ((await res.json()) as T)
}

function formatWhen(iso: string | null): string {
  if (!iso) return 'never connected'
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString()
}

/** Name the device, then show its pairing link + QR code exactly once. */
function PairDeviceModal({ onClose, onPaired }: { onClose: () => void; onPaired: () => void }) {
  const [name, setName] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const [pairing, setPairing] = useState<Pairing | null>(null)
  const [copied, setCopied] = useState(false)
  const disabledReason = name.trim() ? '' : 'Name the device first.'

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    if (disabledReason) return
    setBusy(true)
    setError('')
    try {
      const p = await api<Pairing>('/api/remote-access/devices', {
        method: 'POST',
        body: JSON.stringify({ name: name.trim() }),
      })
      setPairing(p)
      onPaired()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to pair device')
    } finally {
      setBusy(false)
    }
  }

  if (pairing) {
    const command = `peckboard-connect ${pairing.pairing_link}`
    return (
      <Modal onClose={onClose} maxWidth={560} data-testid="remote-pair-link-modal">
        <h2>Pair {pairing.device.name}</h2>
        <p className="form-hint">
          This link is shown <strong>only once</strong> — it holds the device&rsquo;s secret. Anyone
          with it can reach this Peckboard while remote access is on. Revoke the device to
          invalidate it.
        </p>
        {pairing.qr_svg && (
          <div
            className="mfa-qr"
            data-testid="remote-pair-qr"
            dangerouslySetInnerHTML={{ __html: pairing.qr_svg }}
          />
        )}
        <div className="form-field">
          <label className="form-label" htmlFor="remote-pair-link">
            Pairing link
          </label>
          <textarea
            id="remote-pair-link"
            className="form-input"
            rows={2}
            readOnly
            value={pairing.pairing_link}
            data-testid="remote-pair-link"
          />
        </div>
        <p className="form-hint">
          Connect with: <code data-testid="remote-pair-command">{command}</code>
        </p>
        <div className="form-actions">
          <button
            type="button"
            className="btn-secondary"
            onClick={() =>
              void copyText(pairing.pairing_link).then((ok) => {
                setCopied(ok)
                if (ok) setTimeout(() => setCopied(false), 2000)
              })
            }
            data-testid="remote-pair-copy"
          >
            {copied ? 'Copied' : 'Copy link'}
          </button>
          <button
            type="button"
            className="btn-primary"
            onClick={onClose}
            data-testid="remote-pair-done"
          >
            Done
          </button>
        </div>
      </Modal>
    )
  }

  return (
    <Modal onClose={onClose} maxWidth={480} data-testid="remote-pair-modal">
      <h2>Pair a Device</h2>
      <p className="form-hint">Each device gets its own secret, so it can be revoked on its own.</p>
      <form onSubmit={handleSubmit}>
        <div className="form-field">
          <label className="form-label" htmlFor="remote-pair-name">
            Device name
          </label>
          <input
            id="remote-pair-name"
            className="form-input"
            type="text"
            value={name}
            autoComplete="off"
            placeholder="e.g. laptop"
            onChange={(e) => {
              setName(e.target.value)
              setError('')
            }}
            autoFocus
            data-testid="remote-pair-name"
          />
          <FieldError message={error} testId="remote-pair-error" />
        </div>
        <div className="form-actions">
          {!busy && disabledReason && <span className="form-actions-reason">{disabledReason}</span>}
          <button type="button" className="btn-secondary" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={busy || !!disabledReason}
            data-testid="remote-pair-submit"
          >
            {busy ? 'Pairing…' : 'Create pairing'}
          </button>
        </div>
      </form>
    </Modal>
  )
}

/**
 * Settings → Remote Access: reach this Peckboard from anywhere through
 * relay.peckboard.com. The relay only introduces the two ends; traffic
 * flows directly (UDP hole punching), end-to-end encrypted. Admin-only —
 * every route behind it is admin-gated server-side.
 */
export default function RemoteAccessSection() {
  const [data, setData] = useState<Overview | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [relayHost, setRelayHost] = useState('')
  const [relayError, setRelayError] = useState('')
  const [saving, setSaving] = useState(false)
  const [confirmEnable, setConfirmEnable] = useState(false)
  const [showPair, setShowPair] = useState(false)
  const [renaming, setRenaming] = useState<RemoteDevice | null>(null)
  const [revoking, setRevoking] = useState<RemoteDevice | null>(null)
  const [revokeBusy, setRevokeBusy] = useState(false)
  const [revokeError, setRevokeError] = useState<string | null>(null)

  const load = useCallback(
    () =>
      api<Overview>('/api/remote-access').then(
        (o) => {
          setData(o)
          setError(null)
          return o
        },
        (e: unknown) => {
          setError(e instanceof Error ? e.message : 'Failed to load remote access')
          return null
        },
      ),
    [],
  )

  useEffect(() => {
    void load().then((o) => o && setRelayHost(o.relay_host))
    const t = setInterval(() => void load(), POLL_MS)
    return () => clearInterval(t)
  }, [load])

  const putSettings = async (body: { enabled?: boolean; relay_host?: string }) => {
    setSaving(true)
    try {
      await api('/api/remote-access', { method: 'PUT', body: JSON.stringify(body) })
      await load()
      return true
    } catch (e) {
      const msg = e instanceof Error ? e.message : 'Failed to save'
      if (body.relay_host !== undefined) setRelayError(msg)
      else setError(msg)
      return false
    } finally {
      setSaving(false)
    }
  }

  const buildMenu = (d: RemoteDevice): MenuItem[] => [
    { label: 'Rename', onSelect: () => setRenaming(d), testId: `remote-device-rename-${d.id}` },
    { divider: true },
    {
      label: 'Revoke',
      danger: true,
      onSelect: () => {
        setRevokeError(null)
        setRevoking(d)
      },
      testId: `remote-device-revoke-${d.id}`,
    },
  ]

  const enabled = data?.enabled ?? false
  const relayDirty = data !== null && relayHost.trim() !== data.relay_host

  return (
    <section className="settings-section" data-testid="remote-access-section">
      <div className="settings-section-head">
        <h3>Remote Access</h3>
        <div className="acct-row-actions">
          <button
            type="button"
            className="btn-primary btn-sm"
            onClick={() => setShowPair(true)}
            data-testid="remote-pair-device"
          >
            + Pair device
          </button>
        </div>
      </div>
      <p className="form-hint">
        Reach this Peckboard from anywhere through the relay. The relay only introduces your device
        to this server; traffic then flows directly between the two, end-to-end encrypted. Every
        paired device gets the same access as your browser.
      </p>
      <div className="theme-toggle">
        <button
          className={`theme-btn ${!enabled ? 'active' : ''}`}
          onClick={() => enabled && void putSettings({ enabled: false })}
          disabled={saving || !data}
          data-testid="remote-access-off"
        >
          Off
        </button>
        <button
          className={`theme-btn ${enabled ? 'active' : ''}`}
          onClick={() => !enabled && setConfirmEnable(true)}
          disabled={saving || !data}
          data-testid="remote-access-on"
        >
          On
        </button>
      </div>

      <div className="form-field">
        <label className="form-label" htmlFor="remote-relay-host">
          Relay host
        </label>
        <div className="settings-row">
          <input
            id="remote-relay-host"
            className="form-input"
            type="text"
            value={relayHost}
            autoComplete="off"
            onChange={(e) => {
              setRelayHost(e.target.value)
              setRelayError('')
            }}
            data-testid="remote-relay-host"
          />
          <button
            type="button"
            className="btn-secondary btn-sm"
            disabled={saving || !relayDirty}
            onClick={() => void putSettings({ relay_host: relayHost.trim() })}
            data-testid="remote-relay-save"
          >
            Save
          </button>
        </div>
        <FieldError message={relayError} testId="remote-relay-error" />
      </div>

      {error && (
        <p className="form-error" role="alert" data-testid="remote-access-error">
          {error}
        </p>
      )}

      <List<RemoteDevice>
        items={data?.devices ?? []}
        getKey={(d) => d.id}
        bodyClassName="list-view-rows"
        onActivate={(d) => setRenaming(d)}
        getMenuItems={buildMenu}
        renderItem={(d) => (
          <>
            <span className="list-view-name" data-testid={`remote-device-row-${d.name}`}>
              {d.name}
            </span>
            <span className="list-view-meta">
              <span
                className="list-view-tag"
                title={d.status.error ?? d.status.peer ?? undefined}
                data-testid={`remote-device-state-${d.name}`}
              >
                {enabled ? STATE_LABEL[d.status.state] : 'off'}
              </span>
              <span>{formatWhen(d.last_connected_at)}</span>
            </span>
          </>
        )}
        emptyState={
          <div className="list-view-empty" data-testid="remote-devices-empty">
            {data ? 'No paired devices yet.' : 'Loading…'}
          </div>
        }
      />

      {showPair && (
        <PairDeviceModal onClose={() => setShowPair(false)} onPaired={() => void load()} />
      )}
      {renaming && (
        <RenameModal
          title="Rename device"
          label="Device name"
          initialValue={renaming.name}
          onSubmit={async (name) => {
            await api(`/api/remote-access/devices/${renaming.id}`, {
              method: 'PATCH',
              body: JSON.stringify({ name }),
            })
            await load()
          }}
          onClose={() => setRenaming(null)}
        />
      )}
      {confirmEnable && (
        <ConfirmDialog
          title="Turn on remote access?"
          message="This server registers with the relay for every paired device, and anyone holding a device's pairing link can then reach it from the internet (they still have to log in). Revoke devices you no longer use."
          confirmLabel="Turn on"
          cancelLabel="Keep off"
          testId="remote-access-enable-confirm"
          onConfirm={() => {
            setConfirmEnable(false)
            void putSettings({ enabled: true })
          }}
          onCancel={() => setConfirmEnable(false)}
        />
      )}
      {revoking && (
        <ConfirmDialog
          title="Revoke device"
          message={`Revoke "${revoking.name}"? Its pairing secret is destroyed and any open connection is dropped. Pair it again to restore access.`}
          confirmLabel="Revoke"
          danger
          busy={revokeBusy}
          busyLabel="Revoking…"
          error={revokeError}
          testId="remote-device-revoke-confirm"
          onConfirm={() => {
            const target = revoking
            setRevokeBusy(true)
            void api(`/api/remote-access/devices/${target.id}`, { method: 'DELETE' })
              .then(async () => {
                setRevoking(null)
                await load()
              })
              .catch((e: unknown) =>
                setRevokeError(e instanceof Error ? e.message : 'Failed to revoke device'),
              )
              .finally(() => setRevokeBusy(false))
          }}
          onCancel={() => setRevoking(null)}
        />
      )}
    </section>
  )
}
