import { useCallback, useEffect, useRef, useState, type FormEvent } from 'react'
import { authedFetch } from '../store/auth'
import List from './List'
import ListViewHeader from './ListViewHeader'
import Modal from './Modal'
import ConfirmDialog from './ConfirmDialog'
import RenameModal from './RenameModal'
import FieldError from './FieldError'
import type { MenuItem } from './Dropdown'

/** Mirrors the backend's `DeviceView` (`src/routes/devices.rs`). `online` /
 *  `in_flight` are the DeviceRegistry's live view, not DB columns. */
export interface AgentDevice {
  id: string
  name: string
  platform: string
  status: string
  last_seen_at: string | null
  created_at: string
  online: boolean
  in_flight: number
}

/** Payload of a `device-update` WS frame (identifiers + counters only). */
interface DeviceUpdate {
  device_id: string
  online: boolean
  in_flight: number
}

const PLATFORMS = [
  { value: 'macos', label: 'macOS' },
  { value: 'linux', label: 'Linux' },
  { value: 'windows', label: 'Windows' },
]

function formatRelative(dateStr: string | null): string {
  if (!dateStr) return 'never'
  const then = new Date(dateStr).getTime()
  if (Number.isNaN(then)) return dateStr
  const seconds = Math.floor((Date.now() - then) / 1000)
  if (seconds < 60) return 'just now'
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes}m ago`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours}h ago`
  const days = Math.floor(hours / 24)
  if (days < 30) return `${days}d ago`
  return new Date(dateStr).toLocaleString()
}

/**
 * The Agents panel — remote-control devices enrolled with this Peckboard.
 * Lists each device with its live online/in-flight state, offers enrolment
 * (the one-time token reveal), rename, the disable kill-switch, and delete
 * (revoke). Live updates arrive as `peckboard:device-update` CustomEvents
 * re-dispatched by the WS store.
 */
export default function AgentsView() {
  const [devices, setDevices] = useState<AgentDevice[]>([])
  const [loaded, setLoaded] = useState(false)
  const [showEnroll, setShowEnroll] = useState(false)
  const [renaming, setRenaming] = useState<AgentDevice | null>(null)
  const [confirmDelete, setConfirmDelete] = useState<AgentDevice | null>(null)
  const [deleteBusy, setDeleteBusy] = useState(false)
  const [deleteError, setDeleteError] = useState<string | null>(null)
  const [activityFor, setActivityFor] = useState<AgentDevice | null>(null)

  const fetchDevices = useCallback(async () => {
    try {
      const res = await authedFetch('/api/devices')
      if (!res.ok) return
      const body = await res.json()
      // Revoked rows are kept server-side for the audit trail; the panel
      // only shows devices the user can still act on.
      setDevices(((body.devices ?? []) as AgentDevice[]).filter((d) => d.status !== 'revoked'))
    } finally {
      setLoaded(true)
    }
  }, [])

  useEffect(() => {
    void fetchDevices()
  }, [fetchDevices])

  // Live updates: patch online/in-flight in place immediately, then a
  // trailing refetch picks up metadata the frame doesn't carry (a rename or
  // status flip made in another tab). The debounce keeps a burst of
  // in-flight ticks from turning into a fetch storm.
  const refetchTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  useEffect(() => {
    const onUpdate = (e: Event) => {
      const msg = (e as CustomEvent).detail
      const data = (msg?.data ?? msg?.event) as DeviceUpdate | undefined
      if (!data?.device_id) return
      setDevices((prev) =>
        prev.map((d) =>
          d.id === data.device_id ? { ...d, online: data.online, in_flight: data.in_flight } : d,
        ),
      )
      if (refetchTimer.current) clearTimeout(refetchTimer.current)
      refetchTimer.current = setTimeout(() => void fetchDevices(), 300)
    }
    window.addEventListener('peckboard:device-update', onUpdate)
    return () => {
      window.removeEventListener('peckboard:device-update', onUpdate)
      if (refetchTimer.current) clearTimeout(refetchTimer.current)
    }
  }, [fetchDevices])

  const patchDevice = async (id: string, body: { name?: string; status?: string }) => {
    const res = await authedFetch(`/api/devices/${id}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    })
    if (!res.ok) {
      const msg = (await res.json().catch(() => null))?.error
      throw new Error(msg ?? `Update failed (${res.status})`)
    }
    const updated = (await res.json()).device as AgentDevice
    setDevices((prev) => prev.map((d) => (d.id === updated.id ? updated : d)))
  }

  const handleDelete = async () => {
    if (!confirmDelete) return
    setDeleteBusy(true)
    setDeleteError(null)
    try {
      const res = await authedFetch(`/api/devices/${confirmDelete.id}`, { method: 'DELETE' })
      if (!res.ok) throw new Error(`Delete failed (${res.status})`)
      setDevices((prev) => prev.filter((d) => d.id !== confirmDelete.id))
      setConfirmDelete(null)
    } catch (err) {
      setDeleteError(err instanceof Error ? err.message : 'Delete failed')
    } finally {
      setDeleteBusy(false)
    }
  }

  const buildMenu = (d: AgentDevice): MenuItem[] => [
    { label: 'Recent actions', onSelect: () => setActivityFor(d) },
    { label: 'Rename', onSelect: () => setRenaming(d) },
    {
      label: d.status === 'disabled' ? 'Enable' : 'Disable',
      onSelect: () =>
        void patchDevice(d.id, {
          status: d.status === 'disabled' ? 'active' : 'disabled',
        }).catch(() => void fetchDevices()),
    },
    { divider: true },
    { label: 'Delete', danger: true, onSelect: () => setConfirmDelete(d) },
  ]

  return (
    <div className="list-view" data-testid="agents-view">
      <ListViewHeader
        title="Agents"
        actionLabel="+ Enroll agent"
        onAction={() => setShowEnroll(true)}
        actionTestId="agents-enroll"
      />

      {!loaded ? (
        <div className="list-view-body">
          <div className="list-view-empty">Loading…</div>
        </div>
      ) : (
        <List<AgentDevice>
          items={devices}
          getKey={(d) => d.id}
          onActivate={(d) => setActivityFor(d)}
          getMenuItems={buildMenu}
          renderItem={(d) => (
            <>
              <span
                className={`status-dot ${d.online ? 'status-dot-working' : 'status-dot-idle'}`}
                title={d.online ? 'Online' : 'Offline'}
                data-testid={`agent-dot-${d.online ? 'online' : 'offline'}`}
              />
              <span className="list-view-name">{d.name}</span>
              <span className="list-view-meta">
                {d.status === 'disabled' && (
                  <span className="status-badge status-paused" data-testid="agent-disabled-badge">
                    disabled
                  </span>
                )}
                {d.in_flight > 0 && (
                  <span className="list-view-tag" data-testid="agent-in-flight">
                    {d.in_flight} in flight
                  </span>
                )}
                <span className="list-view-tag">
                  {PLATFORMS.find((p) => p.value === d.platform)?.label ?? d.platform}
                </span>
                <span className="list-view-time">
                  {d.online ? 'online' : `seen ${formatRelative(d.last_seen_at)}`}
                </span>
              </span>
            </>
          )}
          emptyState={
            <div className="list-view-empty" data-testid="agents-empty">
              <p>No remote agents enrolled yet</p>
              <p>
                Enroll a machine to control it from Peckboard — run commands, manage servers, and
                drive its screen from your sessions.
              </p>
              <button className="list-view-empty-action" onClick={() => setShowEnroll(true)}>
                Enroll your first agent
              </button>
            </div>
          }
        />
      )}

      {showEnroll && (
        <EnrollModal
          onClose={() => setShowEnroll(false)}
          onEnrolled={(device) => setDevices((prev) => [device, ...prev])}
        />
      )}

      {renaming && (
        <RenameModal
          title="Rename agent"
          label="Agent name"
          initialValue={renaming.name}
          onSubmit={(name) => patchDevice(renaming.id, { name })}
          onClose={() => setRenaming(null)}
        />
      )}

      {confirmDelete && (
        <ConfirmDialog
          title="Delete agent"
          message={`Delete "${confirmDelete.name}"? Its enrollment token stops working immediately and the daemon can never reconnect. This cannot be undone.`}
          confirmLabel="Delete"
          cancelLabel="Cancel"
          danger
          busy={deleteBusy}
          error={deleteError}
          onConfirm={() => void handleDelete()}
          onCancel={() => {
            setConfirmDelete(null)
            setDeleteError(null)
          }}
        />
      )}

      {activityFor && <ActivityModal device={activityFor} onClose={() => setActivityFor(null)} />}
    </div>
  )
}

/**
 * Enrollment dialog. Step 1 collects name + platform; step 2 reveals the
 * one-time enrollment token — the only moment the plaintext ever exists
 * client-side, so the copy affordance and the "shown once" warning live
 * here.
 */
function EnrollModal({
  onClose,
  onEnrolled,
}: {
  onClose: () => void
  onEnrolled: (device: AgentDevice) => void
}) {
  const [name, setName] = useState('')
  const [platform, setPlatform] = useState('linux')
  const [nameError, setNameError] = useState('')
  const [submitError, setSubmitError] = useState('')
  const [busy, setBusy] = useState(false)
  const [token, setToken] = useState<string | null>(null)
  const [copied, setCopied] = useState(false)
  const [copiedCmd, setCopiedCmd] = useState(false)

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    const trimmed = name.trim()
    if (!trimmed) {
      setNameError('Name cannot be empty')
      return
    }
    setBusy(true)
    setSubmitError('')
    try {
      const res = await authedFetch('/api/devices', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name: trimmed, platform }),
      })
      if (!res.ok) {
        const msg = (await res.json().catch(() => null))?.error
        throw new Error(msg ?? `Enroll failed (${res.status})`)
      }
      const body = await res.json()
      onEnrolled(body.device as AgentDevice)
      setToken(body.enrollment_token as string)
    } catch (err) {
      setSubmitError(err instanceof Error ? err.message : 'Enroll failed')
    } finally {
      setBusy(false)
    }
  }

  const copy = async (text: string, mark: (v: boolean) => void) => {
    try {
      await navigator.clipboard.writeText(text)
      mark(true)
      setTimeout(() => mark(false), 2000)
    } catch {
      // Clipboard can be unavailable (http, permissions); the token and
      // command stay visible for manual selection.
    }
  }

  if (token) {
    // The exact command the user runs on the machine being enrolled — the
    // only moment the plaintext token exists client-side.
    const installCommand = `peckboard-agent enroll --server ${window.location.origin} --token ${token}`
    return (
      <Modal onClose={onClose} maxWidth={520} data-testid="enroll-token-modal">
        <h2>Agent enrolled</h2>
        <p>
          Run this on the machine to connect it. The enrollment token is shown{' '}
          <strong>only once</strong> and cannot be recovered — if it's lost, delete the agent and
          enroll again.
        </p>
        <div className="form-field">
          <label className="form-label">Install command</label>
          <code data-testid="enroll-command" style={{ wordBreak: 'break-all', userSelect: 'all' }}>
            {installCommand}
          </code>
        </div>
        <div className="form-field">
          <label className="form-label">Enrollment token</label>
          <code data-testid="enroll-token" style={{ wordBreak: 'break-all', userSelect: 'all' }}>
            {token}
          </code>
        </div>
        <div className="form-actions">
          <button
            type="button"
            className="btn-secondary"
            onClick={() => void copy(installCommand, setCopiedCmd)}
            data-testid="enroll-command-copy"
          >
            {copiedCmd ? 'Copied!' : 'Copy command'}
          </button>
          <button
            type="button"
            className="btn-secondary"
            onClick={() => void copy(token, setCopied)}
            data-testid="enroll-token-copy"
          >
            {copied ? 'Copied!' : 'Copy token'}
          </button>
          <button type="button" className="btn-primary" onClick={onClose} data-testid="enroll-done">
            Done
          </button>
        </div>
      </Modal>
    )
  }

  return (
    <Modal onClose={onClose} maxWidth={420} data-testid="enroll-modal">
      <h2>Enroll agent</h2>
      <form onSubmit={handleSubmit}>
        <div className="form-field">
          <label className="form-label" htmlFor="enroll-name">
            Machine name
          </label>
          <input
            id="enroll-name"
            className="form-input"
            type="text"
            value={name}
            autoFocus
            placeholder="e.g. Work laptop"
            onChange={(e) => {
              setName(e.target.value)
              if (nameError) setNameError('')
            }}
            data-testid="enroll-name"
          />
          <FieldError message={nameError} testId="enroll-name-error" />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="enroll-platform">
            Platform
          </label>
          <select
            id="enroll-platform"
            className="form-input"
            value={platform}
            onChange={(e) => setPlatform(e.target.value)}
            data-testid="enroll-platform"
          >
            {PLATFORMS.map((p) => (
              <option key={p.value} value={p.value}>
                {p.label}
              </option>
            ))}
          </select>
        </div>
        <FieldError message={submitError} testId="enroll-error" />
        <div className="form-actions">
          <button type="button" className="btn-secondary" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button type="submit" className="btn-primary" disabled={busy} data-testid="enroll-submit">
            {busy ? 'Enrolling…' : 'Enroll'}
          </button>
        </div>
      </form>
    </Modal>
  )
}

/** One row of `GET /api/devices/:id/activity` (backend `DeviceActivity`). */
interface ActivityRow {
  id: string
  session_id: string | null
  capability: string
  args_summary: string
  status: string
  created_at: string
}

/**
 * Recent-actions drawer: the device's audit log of bridged remote-agent
 * actions, newest first. Read-only — rendered from
 * `GET /api/devices/:id/activity`; the backend caps it at 50 rows.
 */
function ActivityModal({ device, onClose }: { device: AgentDevice; onClose: () => void }) {
  const [rows, setRows] = useState<ActivityRow[] | null>(null)
  const [error, setError] = useState('')

  useEffect(() => {
    let cancelled = false
    void (async () => {
      try {
        const res = await authedFetch(`/api/devices/${device.id}/activity`)
        if (!res.ok) throw new Error(`Failed to load activity (${res.status})`)
        const body = await res.json()
        if (!cancelled) setRows((body.activity ?? []) as ActivityRow[])
      } catch (err) {
        if (!cancelled) setError(err instanceof Error ? err.message : 'Failed to load activity')
      }
    })()
    return () => {
      cancelled = true
    }
  }, [device.id])

  return (
    <Modal onClose={onClose} maxWidth={560} data-testid="agent-activity-modal">
      <h2>Recent actions — {device.name}</h2>
      {error ? (
        <FieldError message={error} testId="agent-activity-error" />
      ) : rows === null ? (
        <div className="list-view-empty">Loading…</div>
      ) : rows.length === 0 ? (
        <div className="list-view-empty" data-testid="agent-activity-empty">
          <p>No actions yet</p>
          <p>Actions sessions run on this machine appear here, newest first.</p>
        </div>
      ) : (
        <div data-testid="agent-activity-list">
          {rows.map((r) => (
            <div key={r.id} className="list-view-row" data-testid="agent-activity-row">
              <span className="list-view-name">
                {r.capability}
                <code style={{ display: 'block', fontSize: 11, wordBreak: 'break-all' }}>
                  {r.args_summary}
                </code>
              </span>
              <span className="list-view-meta">
                <span
                  className={`status-badge ${r.status === 'ok' ? 'status-active' : 'status-failed'}`}
                >
                  {r.status}
                </span>
                {r.session_id && (
                  <span className="list-view-tag" title={r.session_id}>
                    session {r.session_id.slice(0, 8)}
                  </span>
                )}
                <span className="list-view-time">{formatRelative(r.created_at)}</span>
              </span>
            </div>
          ))}
        </div>
      )}
      <div className="form-actions">
        <button type="button" className="btn-primary" onClick={onClose}>
          Close
        </button>
      </div>
    </Modal>
  )
}
