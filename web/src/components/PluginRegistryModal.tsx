import { useCallback, useEffect, useState } from 'react'
import Modal from './Modal'
import {
  addRepository,
  fetchRegistry,
  HOOK_DESCRIPTIONS,
  installRegistryPlugin,
  removeRepository,
  type RegistryData,
  type RegistryPlugin,
  type RegistryRepo,
} from '../utils/pluginApproval'

type Tab = 'plugins' | 'repositories'

/**
 * The Plugin Registry page (its own modal, reached from Settings →
 * Plugins → "Browse plugins"). Two tabs: **Plugins** — every plugin
 * aggregated across all configured repositories, searchable, with an
 * Install button — and **Repositories** — manage the registry sources.
 * One `/api/plugins/registry` call powers both tabs (it returns the
 * repositories with reachability AND the merged plugin list).
 */
export default function PluginRegistryModal({ onClose }: { onClose: () => void }) {
  const [tab, setTab] = useState<Tab>('plugins')
  const [data, setData] = useState<RegistryData | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [query, setQuery] = useState('')

  const load = useCallback(() => {
    fetchRegistry()
      .then((d) => {
        setData(d)
        setError(null)
      })
      .catch((e: Error) => setError(e.message))
  }, [])

  useEffect(() => {
    load()
    const onApproval = () => load()
    window.addEventListener('peckboard:plugin-approval', onApproval)
    return () => window.removeEventListener('peckboard:plugin-approval', onApproval)
  }, [load])

  return (
    <Modal
      onClose={onClose}
      className="plugins-modal"
      maxWidth={760}
      data-testid="plugin-registry-modal"
    >
      <h2>Plugin Registry</h2>
      <div className="registry-tabs" role="tablist">
        <button
          type="button"
          role="tab"
          aria-selected={tab === 'plugins'}
          className={`registry-tab ${tab === 'plugins' ? 'registry-tab--active' : ''}`}
          data-testid="registry-tab-plugins"
          onClick={() => setTab('plugins')}
        >
          Plugins
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={tab === 'repositories'}
          className={`registry-tab ${tab === 'repositories' ? 'registry-tab--active' : ''}`}
          data-testid="registry-tab-repositories"
          onClick={() => setTab('repositories')}
        >
          Repositories
        </button>
      </div>

      {error && <p className="plugin-card-error">{error}</p>}
      {data === null && !error && <p className="settings-loading">Loading…</p>}

      {data && tab === 'plugins' && (
        <PluginsTab data={data} query={query} setQuery={setQuery} onChanged={load} />
      )}
      {data && tab === 'repositories' && <RepositoriesTab data={data} onChanged={load} />}

      <div className="form-actions">
        <button type="button" className="btn-secondary" onClick={onClose}>
          Close
        </button>
      </div>
    </Modal>
  )
}

function matchesQuery(p: RegistryPlugin, q: string): boolean {
  if (!q) return true
  const hay = [p.name, p.id, p.description, p.repository_label, ...p.hooks].join(' ').toLowerCase()
  return hay.includes(q.toLowerCase())
}

function PluginsTab({
  data,
  query,
  setQuery,
  onChanged,
}: {
  data: RegistryData
  query: string
  setQuery: (q: string) => void
  onChanged: () => void
}) {
  const [busy, setBusy] = useState<string | null>(null)
  const [installError, setInstallError] = useState<string | null>(null)

  const install = (p: RegistryPlugin) => {
    setBusy(p.id)
    setInstallError(null)
    installRegistryPlugin(p.id, p.repository)
      .then((res) =>
        res.ok
          ? res.json()
          : res
              .json()
              .catch(() => null)
              .then((b: { error?: string } | null) =>
                Promise.reject(new Error(b?.error ?? `HTTP ${res.status}`)),
              ),
      )
      .then(() => onChanged())
      .catch((e: Error) => setInstallError(e.message))
      .finally(() => setBusy(null))
  }

  const visible = data.plugins.filter((p) => matchesQuery(p, query))

  return (
    <div className="registry-panel" data-testid="registry-plugins-tab">
      <input
        type="search"
        className="registry-search"
        data-testid="registry-search"
        placeholder="Search plugins…"
        value={query}
        onChange={(e) => setQuery(e.target.value)}
      />
      {installError && <p className="plugin-card-error">{installError}</p>}
      {visible.length === 0 ? (
        <p className="plugin-permissions-empty">
          {data.plugins.length === 0 ? 'No plugins available.' : 'No plugins match your search.'}
        </p>
      ) : (
        <ul className="wasm-plugins-list">
          {visible.map((p) => (
            <li
              key={`${p.repository}:${p.id}`}
              className="wasm-plugin-row"
              data-testid={`registry-plugin-${p.id}`}
              data-installed={p.installed}
            >
              <div className="wasm-plugin-head">
                <span className="wasm-plugin-name">
                  {p.name} <span className="wasm-plugin-version">v{p.version}</span>
                </span>
                {p.installed && (
                  <span className="plugin-badge plugin-badge--approved">Installed</span>
                )}
              </div>
              <div className="registry-plugin-source">{p.repository_label}</div>
              <p className="plugin-card-description">{p.description}</p>
              <ul className="wasm-plugin-hooks">
                {p.hooks.map((h) => (
                  <li key={h} className="wasm-plugin-hook">
                    <code>{h}</code>
                    <span className="wasm-plugin-hook-desc">
                      {HOOK_DESCRIPTIONS[h] ?? 'Custom hook'}
                    </span>
                  </li>
                ))}
              </ul>
              <div className="wasm-plugin-actions">
                <button
                  type="button"
                  className="plugin-approval-approve"
                  data-testid={`registry-install-${p.id}`}
                  disabled={p.installed || busy === p.id}
                  onClick={() => install(p)}
                >
                  {p.installed ? 'Installed' : busy === p.id ? 'Installing…' : 'Install'}
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}

function RepositoriesTab({ data, onChanged }: { data: RegistryData; onChanged: () => void }) {
  const [input, setInput] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const add = () => {
    const value = input.trim()
    if (!value) return
    setBusy(true)
    setError(null)
    addRepository(value)
      .then((res) =>
        res.ok
          ? res.json()
          : res
              .json()
              .catch(() => null)
              .then((b: { error?: string } | null) =>
                Promise.reject(new Error(b?.error ?? `HTTP ${res.status}`)),
              ),
      )
      .then(() => {
        setInput('')
        onChanged()
      })
      .catch((e: Error) => setError(e.message))
      .finally(() => setBusy(false))
  }

  const remove = (url: string) => {
    setError(null)
    removeRepository(url)
      .then((res) =>
        res.ok
          ? res.json()
          : res
              .json()
              .catch(() => null)
              .then((b: { error?: string } | null) =>
                Promise.reject(new Error(b?.error ?? `HTTP ${res.status}`)),
              ),
      )
      .then(() => onChanged())
      .catch((e: Error) => setError(e.message))
  }

  return (
    <div className="registry-panel" data-testid="registry-repositories-tab">
      <div className="registry-repo-add">
        <input
          type="text"
          className="registry-search"
          data-testid="registry-repo-input"
          placeholder="owner/repo or https://…/registry.json"
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') add()
          }}
        />
        <button
          type="button"
          className="plugin-approval-approve"
          data-testid="registry-repo-add"
          disabled={busy || !input.trim()}
          onClick={add}
        >
          Add
        </button>
      </div>
      {error && <p className="plugin-card-error">{error}</p>}
      <ul className="wasm-plugins-list">
        {data.repositories.map((r: RegistryRepo, i: number) => (
          <li
            key={r.url}
            className="wasm-plugin-row"
            data-testid={`registry-repo-${i}`}
            data-repo-url={r.url}
          >
            <div className="wasm-plugin-head">
              <span className="wasm-plugin-name">{r.label}</span>
              {r.ok ? (
                <span className="plugin-badge plugin-badge--approved">OK</span>
              ) : (
                <span className="plugin-badge plugin-badge--init_failed" title={r.error ?? ''}>
                  Unreachable
                </span>
              )}
            </div>
            <div className="registry-plugin-source">{r.url}</div>
            {!r.ok && r.error && <p className="plugin-card-error">{r.error}</p>}
            <div className="wasm-plugin-actions">
              <button
                type="button"
                className="plugin-approval-deny"
                data-testid={`registry-repo-remove-${i}`}
                disabled={!r.removable}
                title={
                  r.removable ? '' : "This source is set by the environment and can't be removed"
                }
                onClick={() => remove(r.url)}
              >
                Remove
              </button>
            </div>
          </li>
        ))}
      </ul>
    </div>
  )
}
