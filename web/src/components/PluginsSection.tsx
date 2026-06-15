import { useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import PluginSettingsModal from './PluginSettingsModal'
import PluginPanelModal from './PluginPanelModal'

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
export default function PluginsSection() {
  const [plugins, setPlugins] = useState<PluginEntry[] | null>(null)
  const [panels, setPanels] = useState<UiPanel[]>([])
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    authedFetch('/api/plugins')
      .then((res) => (res.ok ? res.json() : Promise.reject(new Error(`HTTP ${res.status}`))))
      .then((data: { plugins: PluginEntry[]; ui_panels?: UiPanel[] }) => {
        if (!cancelled) {
          setPlugins(data.plugins)
          setPanels(data.ui_panels ?? [])
        }
      })
      .catch((e: Error) => {
        if (!cancelled) setError(e.message)
      })
    return () => {
      cancelled = true
    }
  }, [])

  return (
    <section className="plugins-section" data-testid="plugins-section">
      {error && <p className="settings-loading">Failed to load plugins: {error}</p>}
      {!error && plugins === null && <p className="settings-loading">Loading plugins…</p>}
      {panels.length > 0 && <PluginPanels panels={panels} />}
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
 * Lists the UI panels any loaded plugin contributes. Generic: the host
 * renders whatever panels a plugin declares and embeds the plugin-served
 * page in a sandboxed iframe — it knows nothing about a panel's contents.
 */
function PluginPanels({ panels }: { panels: UiPanel[] }) {
  const [open, setOpen] = useState<UiPanel | null>(null)
  return (
    <div className="plugin-panels" data-testid="plugin-panels">
      <div className="plugin-panels-title">Plugin Pages</div>
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
        <div className="plugin-permissions-title">Permissions</div>
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
