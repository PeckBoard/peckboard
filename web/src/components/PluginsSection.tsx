import { useCallback, useEffect, useState } from 'react'
import { authedFetch, useAuthStore } from '../store/auth'
import Modal from './Modal'
import PluginPanelModal from './PluginPanelModal'
import ConfirmDialog from './ConfirmDialog'
import HookList from './HookList'
import PermissionList from './PermissionList'
import PluginSettingsForm from './PluginSettingsForm'
import {
  decidePluginApproval,
  uninstallPlugin,
  type WasmPlugin,
  type WasmPluginStats,
} from '../utils/pluginApproval'

interface Permission {
  id: string
  label: string
  description: string
}

interface PluginStatus {
  kind: 'active' | 'init_failed'
  message?: string | null
}

interface SettingField {
  key: string
}

interface PluginEntry {
  id: string
  display_name: string
  description: string
  version: string
  author: string
  built_in: boolean
  permissions: Permission[]
  status: PluginStatus
  enabled: boolean
  settings_schema: { fields: SettingField[] }
}

/**
 * A UI panel a loaded WASM plugin contributes, surfaced in the
 * `/api/plugins` catalog. Opening one embeds the plugin-served page
 * (`path`, always a `/plugin-api/*` route) in a sandboxed iframe.
 */
interface UiPanel {
  plugin: string
  id: string
  title: string
  path: string
}

/**
 * The first sentence of a plugin description, for the compact list rows.
 * Falls back to the whole text when it has no sentence punctuation; the
 * row additionally clamps to one line in CSS.
 */
function firstSentence(text: string): string {
  const m = text.match(/^[^.!?]*[.!?]/)
  return (m ? m[0] : text).trim()
}

/**
 * Lists every plugin (installed WASM + built-in) as a compact row: name,
 * status badge, one-sentence summary, and — for installed plugins — a
 * Remove button. Clicking a row opens a details modal with the full
 * controls. A plugin that declares settings gets its settings form right
 * in that modal — the one place plugin configuration is edited.
 * Settings → Plugin Settings.
 */
export default function PluginsSection({ onBrowseRegistry }: { onBrowseRegistry?: () => void }) {
  const [plugins, setPlugins] = useState<PluginEntry[] | null>(null)
  const [panels, setPanels] = useState<UiPanel[]>([])
  const [wasmPlugins, setWasmPlugins] = useState<WasmPlugin[]>([])
  const [error, setError] = useState<string | null>(null)

  const load = useCallback((signal?: { cancelled: boolean }) => {
    authedFetch('/api/plugins')
      .then((res) => (res.ok ? res.json() : Promise.reject(new Error(`HTTP ${res.status}`))))
      .then(
        (data: { plugins: PluginEntry[]; ui_panels?: UiPanel[]; wasm_plugins?: WasmPlugin[] }) => {
          if (signal?.cancelled) return
          setPlugins(data.plugins)
          setPanels(data.ui_panels ?? [])
          setWasmPlugins(data.wasm_plugins ?? [])
        },
      )
      .catch((e: Error) => {
        if (!signal?.cancelled) setError(e.message)
      })
  }, [])

  useEffect(() => {
    const signal = { cancelled: false }
    load(signal)
    // A decision anywhere re-broadcasts; refresh so status badges stay live.
    const onApproval = () => load()
    window.addEventListener('peckboard:plugin-approval', onApproval)
    return () => {
      signal.cancelled = true
      window.removeEventListener('peckboard:plugin-approval', onApproval)
    }
  }, [load])

  return (
    <section className="plugins-section" data-testid="plugins-section">
      {onBrowseRegistry && (
        <div className="plugins-toolbar">
          <button
            type="button"
            className="plugin-panel-open"
            data-testid="browse-plugins"
            onClick={onBrowseRegistry}
          >
            Browse plugins…
          </button>
        </div>
      )}
      {error && <p className="settings-loading">Failed to load plugins: {error}</p>}
      {!error && plugins === null && <p className="settings-loading">Loading plugins…</p>}
      {wasmPlugins.length > 0 && (
        <WasmPluginList plugins={wasmPlugins} panels={panels} onDecided={() => load()} />
      )}
      {plugins && plugins.length === 0 && <p className="settings-loading">No plugins installed.</p>}
      {plugins && plugins.length > 0 && (
        <div className="plugins-list">
          <div className="plugin-panels-title">Built-in Plugins</div>
          <ul className="wasm-plugins-list">
            {plugins.map((p) => (
              <PluginCard key={p.id} plugin={p} />
            ))}
          </ul>
        </div>
      )}
    </section>
  )
}

function badgeFor(status: WasmPlugin['status']) {
  const label =
    status === 'approved'
      ? 'Approved'
      : status === 'pending'
        ? 'Awaiting approval'
        : status === 'denied'
          ? 'Denied'
          : 'Init failed'
  return <span className={`plugin-badge plugin-badge--${status}`}>{label}</span>
}

/**
 * Compact rows for every loaded WASM plugin: name, approval badge,
 * one-line summary, Remove. The full manifest (hooks, permissions,
 * contributed pages) and the Approve/Deny controls live in the row's
 * details modal — a plugin is inert until its hook set is approved there
 * (or via the startup prompt).
 */
function WasmPluginList({
  plugins,
  panels,
  onDecided,
}: {
  plugins: WasmPlugin[]
  panels: UiPanel[]
  onDecided: () => void
}) {
  const isAdmin = useAuthStore((s) => s.user?.role === 'admin')
  const [busy, setBusy] = useState<string | null>(null)
  const [confirmRemove, setConfirmRemove] = useState<string | null>(null)
  const [detailsFor, setDetailsFor] = useState<WasmPlugin | null>(null)
  // A refused approve/deny/remove parks its reason here and leaves the
  // details modal open, so the operator can't mistake a 4xx/5xx for a
  // decision that took effect (same contract as PluginApprovalPrompt).
  const [error, setError] = useState<string | null>(null)

  const run = async (pluginId: string, call: () => Promise<Response>, verb: string) => {
    setBusy(pluginId)
    setError(null)
    try {
      const res = await call()
      if (!res.ok) {
        const body = (await res.json().catch(() => null)) as { error?: unknown } | null
        throw new Error(typeof body?.error === 'string' ? body.error : `HTTP ${res.status}`)
      }
      onDecided()
      setDetailsFor(null)
    } catch (e) {
      const detail = e instanceof Error ? e.message : 'network error'
      setError(`Could not ${verb} “${pluginId}”: ${detail}`)
    } finally {
      setBusy(null)
    }
  }

  /** Dismiss the details modal, dropping any error it was showing. */
  const closeDetails = () => {
    setDetailsFor(null)
    setError(null)
  }
  const decide = (pluginId: string, decision: 'approve' | 'deny') => {
    void run(pluginId, () => decidePluginApproval(pluginId, decision), decision)
  }

  const remove = (pluginId: string) => {
    setConfirmRemove(null)
    void run(pluginId, () => uninstallPlugin(pluginId), 'remove')
  }

  return (
    <div className="wasm-plugins" data-testid="wasm-plugins">
      <div className="plugin-panels-title">Installed Plugins</div>
      {error && (
        <p className="plugin-card-error" role="alert" data-testid="wasm-plugin-error">
          {error}
        </p>
      )}
      <ul className="wasm-plugins-list">
        {plugins.map((p) => (
          <li
            key={p.name}
            className="wasm-plugin-row plugin-row"
            data-testid={`wasm-plugin-${p.name}`}
            data-status={p.status}
          >
            <button
              type="button"
              className="plugin-row-body"
              data-testid={`wasm-plugin-open-${p.name}`}
              onClick={() => setDetailsFor(p)}
            >
              <span className="wasm-plugin-name">{p.name}</span>
              {badgeFor(p.status)}
              <span className="plugin-row-summary">{firstSentence(p.description)}</span>
              {p.stats && <PluginUsageChips stats={p.stats} />}
            </button>
            {isAdmin && (
              <button
                type="button"
                className="plugin-approval-remove"
                data-testid={`wasm-plugin-remove-${p.name}`}
                disabled={busy === p.name}
                onClick={() => setConfirmRemove(p.name)}
              >
                Remove
              </button>
            )}
          </li>
        ))}
      </ul>
      {detailsFor && (
        <Modal
          onClose={() => closeDetails()}
          className="plugin-details-modal"
          maxWidth={560}
          data-testid={`plugin-details-${detailsFor.name}`}
        >
          <header className="plugin-details-head">
            <h2>{detailsFor.name}</h2>
            {badgeFor(detailsFor.status)}
          </header>
          <div className="plugin-card-meta">
            <span>v{detailsFor.version}</span>
            <span>·</span>
            <SourceRepo repository={detailsFor.repository} />
          </div>
          <p className="plugin-card-description">{detailsFor.description}</p>
          {detailsFor.status === 'init_failed' && detailsFor.error && (
            <p className="plugin-card-error">{detailsFor.error}</p>
          )}
          <HookList hooks={detailsFor.hooks} title="Hooks" />
          <PermissionList permissions={detailsFor.permissions} title="Permissions" />
          <PluginUsageBlock stats={detailsFor.stats} />
          <PluginPanelList panels={panels.filter((panel) => panel.plugin === detailsFor.name)} />
          {detailsFor.status === 'approved' &&
            (detailsFor.settings_schema?.fields?.length ?? 0) > 0 && (
              <div
                className="plugin-details-settings"
                data-testid={`plugin-settings-entry-${detailsFor.name}`}
              >
                <div className="plugin-section-title">Settings</div>
                <PluginSettingsForm pluginId={detailsFor.name} />
              </div>
            )}
          {error && (
            <p className="plugin-card-error" role="alert" data-testid="plugin-details-error">
              {error}
            </p>
          )}
          <div className="form-actions">
            {isAdmin && detailsFor.status !== 'approved' && (
              <button
                type="button"
                className="plugin-approval-approve"
                data-testid={`wasm-plugin-approve-${detailsFor.name}`}
                disabled={busy === detailsFor.name}
                onClick={() => decide(detailsFor.name, 'approve')}
              >
                Approve
              </button>
            )}
            {isAdmin && detailsFor.status !== 'denied' && (
              <button
                type="button"
                className="plugin-approval-deny"
                data-testid={`wasm-plugin-deny-${detailsFor.name}`}
                disabled={busy === detailsFor.name}
                onClick={() => decide(detailsFor.name, 'deny')}
              >
                {detailsFor.status === 'approved' ? 'Revoke' : 'Deny'}
              </button>
            )}
            <button type="button" className="btn-secondary" onClick={() => closeDetails()}>
              Close
            </button>
          </div>
        </Modal>
      )}
      {confirmRemove && (
        <ConfirmDialog
          title="Remove plugin"
          message={`Remove the “${confirmRemove}” plugin? This shuts it down, deletes it from disk, and clears its approval and settings. You can reinstall it later from the registry.`}
          confirmLabel="Remove"
          danger
          onConfirm={() => remove(confirmRemove)}
          onCancel={() => setConfirmRemove(null)}
        />
      )}
    </div>
  )
}

/**
 * A plugin's source repository (a required manifest field), rendered as a
 * link to the repo when it's an http(s) URL, or plain text otherwise. The
 * label drops the scheme so it reads as `host/owner/repo`.
 */
function SourceRepo({ repository }: { repository: string }) {
  const label = repository.replace(/^https?:\/\//, '').replace(/\/+$/, '')
  if (!/^https?:\/\//.test(repository)) {
    return <span className="wasm-plugin-repo">{label}</span>
  }
  return (
    <a className="wasm-plugin-repo" href={repository} target="_blank" rel="noreferrer noopener">
      {label}
    </a>
  )
}

/** Human-readable byte size: 2.3 MB, 412 kB. */
function fmtBytes(n: number): string {
  if (n <= 0) return '—'
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(n < 10240 ? 1 : 0)} kB`
  return `${(n / (1024 * 1024)).toFixed(1)} MB`
}

/** Human-readable duration from milliseconds: 840 ms, 2.1 s, 3.5 min. */
function fmtDuration(ms: number): string {
  if (ms < 1000) return `${ms} ms`
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)} s`
  return `${(ms / 60_000).toFixed(1)} min`
}

/**
 * Compact per-plugin resource usage on the row: wasm size, call count, and
 * total execution time, plus an error chip only when something failed.
 * Counters are per-process — they reset when the server restarts or the
 * plugin is reinstalled.
 */
function PluginUsageChips({ stats }: { stats: WasmPluginStats }) {
  return (
    <span className="wasm-plugin-usage" data-testid="wasm-plugin-usage">
      <span className="wasm-plugin-usage-chip">{fmtBytes(stats.wasm_bytes)}</span>
      <span className="wasm-plugin-usage-chip">
        {stats.calls.toLocaleString()} {stats.calls === 1 ? 'call' : 'calls'}
      </span>
      <span className="wasm-plugin-usage-chip">{fmtDuration(stats.busy_ms)} busy</span>
      {stats.errors > 0 && (
        <span className="wasm-plugin-usage-chip wasm-plugin-usage-chip--errors">
          {stats.errors.toLocaleString()} {stats.errors === 1 ? 'error' : 'errors'}
        </span>
      )}
    </span>
  )
}

/**
 * Fuller usage list for the details modal: every counter the host tracks,
 * including instance rebuilds (the self-healing path) and the last call.
 */
function PluginUsageBlock({ stats }: { stats?: WasmPluginStats | null }) {
  if (!stats) return null
  const rows: [string, string][] = [
    ['Wasm size', fmtBytes(stats.wasm_bytes)],
    ['Calls', stats.calls.toLocaleString()],
    ['Errors', stats.errors.toLocaleString()],
    ['Instance rebuilds', stats.rebuilds.toLocaleString()],
    ['Total execution time', fmtDuration(stats.busy_ms)],
    [
      'Last call',
      stats.last_call_ms == null
        ? 'never'
        : `${fmtDuration(stats.last_call_ms)}${
            stats.last_call_at ? ` · ${new Date(stats.last_call_at).toLocaleString()}` : ''
          }`,
    ],
  ]
  return (
    <div className="plugin-usage" data-testid="plugin-usage">
      <div className="plugin-panels-title" title="Counted since the server started">
        Resource usage
      </div>
      <dl className="plugin-usage-grid">
        {rows.map(([label, value]) => (
          <div key={label} className="plugin-usage-item">
            <dt>{label}</dt>
            <dd>{value}</dd>
          </div>
        ))}
      </dl>
    </div>
  )
}

/**
 * The UI pages a single plugin contributes, rendered inside that plugin's
 * details modal: one titled "Open" button per page, each embedding the
 * plugin-served page in a sandboxed iframe. Generic — the host knows
 * nothing about a page's contents. Renders nothing when the plugin
 * contributes no pages.
 */
function PluginPanelList({ panels }: { panels: UiPanel[] }) {
  const [open, setOpen] = useState<UiPanel | null>(null)
  if (panels.length === 0) return null
  return (
    <div className="plugin-panels" data-testid="plugin-panels">
      <ul className="plugin-panels-list">
        {panels.map((panel) => (
          <li key={`${panel.plugin}:${panel.id}`} className="plugin-panel-row">
            <span className="plugin-panel-row-title">{panel.title}</span>
            <button
              type="button"
              className="plugin-panel-open"
              data-testid={`plugin-panel-open-${panel.plugin}-${panel.id}`}
              onClick={() => setOpen(panel)}
            >
              Open
            </button>
          </li>
        ))}
      </ul>
      {open && (
        <PluginPanelModal
          title={open.title}
          plugin={open.plugin}
          path={open.path}
          onClose={() => setOpen(null)}
        />
      )}
    </div>
  )
}

/**
 * A built-in plugin as a compact row; the details modal carries the full
 * description, the built-in tag, and the permission grants. Built-ins
 * can't be removed and their settings live on Settings → Plugin Settings.
 */
function PluginCard({ plugin }: { plugin: PluginEntry }) {
  const [detailsOpen, setDetailsOpen] = useState(false)
  const statusBadge =
    plugin.status.kind === 'active' ? (
      <span className="plugin-badge plugin-badge--active">Active</span>
    ) : (
      <span className="plugin-badge plugin-badge--failed" title={plugin.status.message ?? ''}>
        Init failed
      </span>
    )
  return (
    <li
      className="plugin-card plugin-row"
      data-testid={`plugin-card-${plugin.id}`}
      data-plugin-id={plugin.id}
    >
      <button
        type="button"
        className="plugin-row-body"
        data-testid={`plugin-open-${plugin.id}`}
        onClick={() => setDetailsOpen(true)}
      >
        <span className="wasm-plugin-name">{plugin.display_name}</span>
        {statusBadge}
        <span className="plugin-row-summary">{firstSentence(plugin.description)}</span>
      </button>
      {detailsOpen && (
        <Modal
          onClose={() => setDetailsOpen(false)}
          className="plugin-details-modal"
          maxWidth={560}
          data-testid={`plugin-details-${plugin.id}`}
        >
          <header className="plugin-details-head">
            <h2>{plugin.display_name}</h2>
            {statusBadge}
          </header>
          <div className="plugin-card-meta">
            <span>v{plugin.version}</span>
            <span>·</span>
            <span>{plugin.author}</span>
            {plugin.built_in && (
              <>
                <span>·</span>
                <span className="plugin-card-meta-builtin">Built-in · always enabled</span>
              </>
            )}
          </div>
          <p className="plugin-card-description">{plugin.description}</p>
          {plugin.status.kind === 'init_failed' && plugin.status.message && (
            <p className="plugin-card-error">{plugin.status.message}</p>
          )}
          <div className="plugin-permissions">
            <div className="plugin-section-title">Permissions</div>
            {plugin.permissions.length === 0 ? (
              <p className="plugin-permissions-empty">No permissions requested.</p>
            ) : (
              <ul className="plugin-permissions-list">
                {plugin.permissions.map((perm) => (
                  <li key={perm.id} className="plugin-permission" data-permission={perm.id}>
                    <span className="plugin-permission-label">{perm.label}</span>
                    <span className="plugin-permission-desc">{perm.description}</span>
                  </li>
                ))}
              </ul>
            )}
          </div>
          {(plugin.settings_schema?.fields?.length ?? 0) > 0 && (
            <div
              className="plugin-details-settings"
              data-testid={`plugin-settings-entry-${plugin.id}`}
            >
              <div className="plugin-section-title">Settings</div>
              <PluginSettingsForm pluginId={plugin.id} />
            </div>
          )}
          <div className="form-actions">
            <button type="button" className="btn-secondary" onClick={() => setDetailsOpen(false)}>
              Close
            </button>
          </div>
        </Modal>
      )}
    </li>
  )
}
