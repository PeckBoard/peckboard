import { useCallback, useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import PluginSettingsModal from './PluginSettingsModal'
import PluginPanelModal from './PluginPanelModal'
import ConfirmDialog from './ConfirmDialog'
import HookList from './HookList'
import PermissionList from './PermissionList'
import { decidePluginApproval, uninstallPlugin, type WasmPlugin } from '../utils/pluginApproval'

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
 * Lists every plugin compiled into Peckboard, with the permissions it
 * was granted and its current status.
 *
 * Today the catalog is read-only: every built-in plugin is always
 * enabled and receives every permission it asks for. The UI is in place
 * so a future enable/disable + grant flow can hook in without another
 * round of design work.
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
          {plugins.map((p) => (
            <PluginCard key={p.id} plugin={p} />
          ))}
        </div>
      )}
    </section>
  )
}

/**
 * Lists every loaded WASM plugin and its approval status, with the hooks
 * it declares and an Approve/Deny control. A plugin is inert until its
 * hook set is approved here (or via the startup prompt).
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
  const [busy, setBusy] = useState<string | null>(null)
  const [confirmRemove, setConfirmRemove] = useState<string | null>(null)

  const decide = (pluginId: string, decision: 'approve' | 'deny') => {
    setBusy(pluginId)
    decidePluginApproval(pluginId, decision)
      .then(() => onDecided())
      .finally(() => setBusy(null))
  }

  const remove = (pluginId: string) => {
    setConfirmRemove(null)
    setBusy(pluginId)
    uninstallPlugin(pluginId)
      .then(() => onDecided())
      .finally(() => setBusy(null))
  }

  const badgeFor = (status: WasmPlugin['status']) => {
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

  return (
    <div className="wasm-plugins" data-testid="wasm-plugins">
      <div className="plugin-panels-title">Installed Plugins</div>
      <ul className="wasm-plugins-list">
        {plugins.map((p) => (
          <li
            key={p.name}
            className="wasm-plugin-row"
            data-testid={`wasm-plugin-${p.name}`}
            data-status={p.status}
          >
            <div className="wasm-plugin-head">
              <div className="wasm-plugin-heading">
                <span className="wasm-plugin-name">{p.name}</span>
                <div className="plugin-card-meta">
                  <span>v{p.version}</span>
                  <span>·</span>
                  <SourceRepo repository={p.repository} />
                </div>
              </div>
              {badgeFor(p.status)}
            </div>
            <p className="plugin-card-description">{p.description}</p>
            <HookList hooks={p.hooks} title="Hooks" />
            <PermissionList permissions={p.permissions} title="Permissions" />
            <PluginPanelList panels={panels.filter((panel) => panel.plugin === p.name)} />
            {p.status === 'init_failed' && p.error && (
              <p className="plugin-card-error">{p.error}</p>
            )}
            <div className="wasm-plugin-actions">
              {p.status !== 'approved' && (
                <button
                  type="button"
                  className="plugin-approval-approve"
                  data-testid={`wasm-plugin-approve-${p.name}`}
                  disabled={busy === p.name}
                  onClick={() => decide(p.name, 'approve')}
                >
                  Approve
                </button>
              )}
              {p.status !== 'denied' && (
                <button
                  type="button"
                  className="plugin-approval-deny"
                  data-testid={`wasm-plugin-deny-${p.name}`}
                  disabled={busy === p.name}
                  onClick={() => decide(p.name, 'deny')}
                >
                  {p.status === 'approved' ? 'Revoke' : 'Deny'}
                </button>
              )}
              <button
                type="button"
                className="plugin-approval-remove"
                data-testid={`wasm-plugin-remove-${p.name}`}
                disabled={busy === p.name}
                onClick={() => setConfirmRemove(p.name)}
              >
                Remove
              </button>
            </div>
          </li>
        ))}
      </ul>
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

/**
 * The UI pages a single plugin contributes, rendered inside that plugin's
 * row: one titled "Open" button per page, each embedding the plugin-served
 * page in a sandboxed iframe. Generic — the host knows nothing about a
 * page's contents. Renders nothing when the plugin contributes no pages.
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

function PluginCard({ plugin }: { plugin: PluginEntry }) {
  const [settingsOpen, setSettingsOpen] = useState(false)
  const hasSettings = (plugin.settings_schema?.fields?.length ?? 0) > 0
  const statusBadge =
    plugin.status.kind === 'active' ? (
      <span className="plugin-badge plugin-badge--active">Active</span>
    ) : (
      <span className="plugin-badge plugin-badge--failed" title={plugin.status.message ?? ''}>
        Init failed
      </span>
    )
  return (
    <article
      className="plugin-card"
      data-testid={`plugin-card-${plugin.id}`}
      data-plugin-id={plugin.id}
    >
      <header className="plugin-card-header">
        <div>
          <h4 className="plugin-card-name">{plugin.display_name}</h4>
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
        </div>
        {statusBadge}
      </header>
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
      {hasSettings && (
        <div className="plugin-card-actions">
          <button
            type="button"
            className="plugin-settings-open"
            data-testid={`plugin-settings-open-${plugin.id}`}
            onClick={() => setSettingsOpen(true)}
          >
            Settings
          </button>
        </div>
      )}
      {settingsOpen && (
        <PluginSettingsModal
          pluginId={plugin.id}
          pluginName={plugin.display_name}
          onClose={() => setSettingsOpen(false)}
        />
      )}
    </article>
  )
}
