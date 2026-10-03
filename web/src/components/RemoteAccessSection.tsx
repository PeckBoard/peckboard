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
  local_port: number | null
  candidates: string[]
  /** While connected: hole-punched, or through the relay. */
  path: 'direct' | 'relayed' | null
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
  udp_port_base: number | null
  udp_port_count: number
  public_address: string
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

/** A failed request; `field` names the rejected input when the server says. */
class ApiError extends Error {
  field?: string
  constructor(message: string, field?: string) {
    super(message)
    this.field = field
  }
}

async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await authedFetch(path, {
    ...init,
    headers: init?.body ? { 'Content-Type': 'application/json' } : undefined,
  })
  if (!res.ok) {
    const body = (await res.json().catch(() => null)) as { error?: string; field?: string } | null
    throw new ApiError(body?.error || `Request failed (${res.status})`, body?.field)
  }
  return res.status === 204 ? (undefined as T) : ((await res.json()) as T)
}

type DirectField = 'udp_port_base' | 'udp_port_count' | 'public_address'
type DirectErrors = Partial<Record<DirectField, string>>

interface DirectForm {
  base: string
  count: string
  publicAddress: string
}

const DIRECT_FIELDS: readonly string[] = ['udp_port_base', 'udp_port_count', 'public_address']

function isDirectField(f: string): f is DirectField {
  return DIRECT_FIELDS.includes(f)
}

function directForm(o: Overview): DirectForm {
  return {
    base: o.udp_port_base?.toString() ?? '',
    count: o.udp_port_count.toString(),
    publicAddress: o.public_address,
  }
}

/** Mirrors the server's `validate_direct`, so errors show while typing. */
function validateDirect(f: DirectForm): DirectErrors {
  const errs: DirectErrors = {}
  const base = f.base.trim()
  const count = f.count.trim()
  const pub = f.publicAddress.trim()
  const b = Number(base)
  const c = Number(count)
  if (base && (!/^\d+$/.test(base) || b < 1024 || b > 65535)) {
    errs.udp_port_base = 'UDP port must be 1024–65535'
  }
  if (!/^\d+$/.test(count) || c < 1 || c > 256) {
    errs.udp_port_count = 'Range size must be 1–256'
  } else if (base && !errs.udp_port_base && b + c - 1 > 65535) {
    errs.udp_port_count = 'Range runs past port 65535'
  }
  if (pub) {
    const ipv6 = /^[0-9a-fA-F:.]+$/.test(pub) && (pub.match(/:/g)?.length ?? 0) >= 2
    const host = /^[A-Za-z0-9.-]+$/.test(pub) && !/^[.-]|[.-]$/.test(pub) && pub.length <= 253
    if (!ipv6 && !host) {
      errs.public_address = 'Public address must be a hostname or IP address, without a port'
    } else if (!base) {
      errs.public_address = 'Set a UDP port first — the public address advertises it'
    }
  }
  return errs
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
  const [direct, setDirect] = useState<DirectForm>({ base: '', count: '10', publicAddress: '' })
  const [directServerErrors, setDirectServerErrors] = useState<DirectErrors>({})

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
    void load().then((o) => {
      if (!o) return
      setRelayHost(o.relay_host)
      setDirect(directForm(o))
    })
    const t = setInterval(() => void load(), POLL_MS)
    return () => clearInterval(t)
  }, [load])

  const putSettings = async (body: {
    enabled?: boolean
    relay_host?: string
    direct?: { udp_port_base: number | null; udp_port_count: number; public_address: string }
  }) => {
    setSaving(true)
    try {
      await api('/api/remote-access', { method: 'PUT', body: JSON.stringify(body) })
      await load()
      return true
    } catch (e) {
      const msg = e instanceof Error ? e.message : 'Failed to save'
      const field = e instanceof ApiError ? e.field : undefined
      if (field && isDirectField(field)) setDirectServerErrors({ [field]: msg })
      else if (body.relay_host !== undefined) setRelayError(msg)
      else setError(msg)
      return false
    } finally {
      setSaving(false)
    }
  }

  const saveDirect = () => {
    const base = direct.base.trim()
    void putSettings({
      direct: {
        udp_port_base: base ? Number(base) : null,
        udp_port_count: Number(direct.count.trim()),
        public_address: direct.publicAddress.trim(),
      },
    })
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
  const directErrors = { ...validateDirect(direct), ...directServerErrors }
  const directInvalid = Object.keys(validateDirect(direct)).length > 0
  const directDirty =
    data !== null &&
    (direct.base.trim() !== (data.udp_port_base?.toString() ?? '') ||
      direct.count.trim() !== data.udp_port_count.toString() ||
      direct.publicAddress.trim() !== data.public_address)
  const editDirect = (patch: Partial<DirectForm>) => {
    setDirect((d) => ({ ...d, ...patch }))
    setDirectServerErrors({})
  }

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
      <div className="settings-subsection" data-testid="remote-direct">
        <h4>Direct Connection (Optional)</h4>
        <p className="form-hint" style={{ marginTop: 0 }}>
          Nothing to set up here — phones connect directly when the network allows and through the
          encrypted relay otherwise. Only change these if you already pin UDP ports on this machine.
        </p>
        <div className="form-field">
          <label className="form-label" htmlFor="remote-udp-base">
            UDP port
          </label>
          <input
            id="remote-udp-base"
            className="form-input"
            type="text"
            inputMode="numeric"
            placeholder="random"
            value={direct.base}
            autoComplete="off"
            onChange={(e) => editDirect({ base: e.target.value })}
            data-testid="remote-udp-base"
          />
          <FieldError message={directErrors.udp_port_base} testId="remote-udp-base-error" />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="remote-udp-count">
            Range size
          </label>
          <input
            id="remote-udp-count"
            className="form-input"
            type="text"
            inputMode="numeric"
            value={direct.count}
            autoComplete="off"
            onChange={(e) => editDirect({ count: e.target.value })}
            data-testid="remote-udp-count"
          />
          <FieldError message={directErrors.udp_port_count} testId="remote-udp-count-error" />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="remote-public-address">
            Public address
          </label>
          <input
            id="remote-public-address"
            className="form-input"
            type="text"
            placeholder="auto (detected via the relay)"
            value={direct.publicAddress}
            autoComplete="off"
            onChange={(e) => editDirect({ publicAddress: e.target.value })}
            data-testid="remote-public-address"
          />
          <FieldError message={directErrors.public_address} testId="remote-public-address-error" />
        </div>
        <div className="form-actions">
          {directDirty && directInvalid && (
            <span className="form-actions-reason">Fix the fields above to save.</span>
          )}
          <button
            type="button"
            className="btn-secondary btn-sm"
            disabled={saving || !directDirty || directInvalid}
            onClick={saveDirect}
            data-testid="remote-direct-save"
          >
            Save
          </button>
        </div>
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
              {enabled && d.status.local_port !== null && (
                <span
                  title={
                    d.status.candidates.length
                      ? `Advertised: ${d.status.candidates.join(', ')}`
                      : undefined
                  }
                  data-testid={`remote-device-port-${d.name}`}
                >
                  UDP {d.status.local_port}
                </span>
              )}
              {enabled && d.status.state === 'connected' && d.status.path === 'relayed' && (
                <span
                  className="list-view-tag"
                  title="No direct path to this device; traffic goes through the relay, still end-to-end encrypted"
                  data-testid={`remote-device-path-${d.name}`}
                >
                  Relayed
                </span>
              )}
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
