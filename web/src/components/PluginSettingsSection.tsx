import { useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import PluginSettingsForm from './PluginSettingsForm'

/**
 * A plugin that declares operator settings, from `/api/plugins` — either
 * a built-in plugin (`plugins[]`, id + display name) or an installed WASM
 * plugin (`wasm_plugins[]`, whose name doubles as its id).
 */
interface SettingsEntry {
  id: string
  name: string
}

interface CatalogBuiltIn {
  id: string
  display_name: string
  settings_schema?: { fields: { key: string }[] }
}

interface CatalogWasm {
  name: string
  settings_schema?: { fields: { key: string }[] }
}

/**
 * Settings → Plugin Settings: the one place plugin configuration is
 * edited. Renders the shared per-plugin settings form for every plugin
 * (built-in and installed) whose manifest declares settings fields. The
 * Plugins sub-page deliberately links nowhere near settings — it stays a
 * compact catalog of what's installed.
 */
export default function PluginSettingsSection() {
  const [entries, setEntries] = useState<SettingsEntry[] | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    authedFetch('/api/plugins')
      .then((res) => (res.ok ? res.json() : Promise.reject(new Error(`HTTP ${res.status}`))))
      .then((data: { plugins?: CatalogBuiltIn[]; wasm_plugins?: CatalogWasm[] }) => {
        if (cancelled) return
        const hasFields = (schema?: { fields: { key: string }[] }) =>
          (schema?.fields?.length ?? 0) > 0
        const builtIns = (data.plugins ?? [])
          .filter((p) => hasFields(p.settings_schema))
          .map((p) => ({ id: p.id, name: p.display_name }))
        const wasm = (data.wasm_plugins ?? [])
          .filter((p) => hasFields(p.settings_schema))
          .map((p) => ({ id: p.name, name: p.name }))
        setEntries([...builtIns, ...wasm])
      })
      .catch((e: Error) => {
        if (!cancelled) setError(e.message)
      })
    return () => {
      cancelled = true
    }
  }, [])

  return (
    <div data-testid="plugin-settings-section">
      {error && <p className="settings-loading">Failed to load plugins: {error}</p>}
      {!error && entries === null && <p className="settings-loading">Loading plugins…</p>}
      {entries && entries.length === 0 && (
        <p className="settings-loading">No installed plugin declares settings.</p>
      )}
      {entries?.map((entry) => (
        <section
          key={entry.id}
          className="settings-section"
          data-testid={`plugin-settings-entry-${entry.id}`}
        >
          <h3>{entry.name}</h3>
          <PluginSettingsForm pluginId={entry.id} />
        </section>
      ))}
    </div>
  )
}
