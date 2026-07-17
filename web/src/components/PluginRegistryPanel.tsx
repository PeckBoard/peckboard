import { useCallback, useEffect, useMemo, useState } from 'react'
import HookList from './HookList'
import {
  addRepository,
  fetchRegistry,
  installRegistryPlugin,
  removeRepository,
  type RegistryData,
  type RegistryMcpServer,
  type RegistryPlugin,
  type RegistryRepo,
} from '../utils/pluginApproval'
import { authedFetch } from '../store/auth'
import { useResourcesStore } from '../store/resources'
import { ServerModal } from './McpServersSection'
import { tidy, type McpServer } from '../utils/mcpServers'

type Tab = 'browse' | 'repositories'
type Kind = 'all' | 'plugins' | 'mcp'

/**
 * The Plugin Registry Settings sub-page. Two tabs: **Browse** — every WASM
 * plugin AND MCP server template aggregated across all configured
 * repositories, with one search box over names/descriptions/tags/categories
 * plus kind, category, and tag filters — and **Repositories** — manage the
 * registry sources. One `/api/plugins/registry` call powers both tabs.
 * Installing a wasm plugin downloads + SHA-256-verifies it server-side;
 * adding an MCP server opens the Settings → MCP Servers editor prefilled
 * from the template (nothing is downloaded).
 */
export default function PluginRegistryPanel() {
  const [tab, setTab] = useState<Tab>('browse')
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
    <div className="registry-panel-page" data-testid="plugin-registry-panel">
      <div className="registry-tabs" role="tablist">
        <button
          type="button"
          role="tab"
          aria-selected={tab === 'browse'}
          className={`registry-tab ${tab === 'browse' ? 'registry-tab--active' : ''}`}
          data-testid="registry-tab-plugins"
          onClick={() => setTab('browse')}
        >
          Browse
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

      {data && tab === 'browse' && (
        <BrowseTab data={data} query={query} setQuery={setQuery} onChanged={load} />
      )}
      {data && tab === 'repositories' && <RepositoriesTab data={data} onChanged={load} />}
    </div>
  )
}

/** Every word of `q` must appear somewhere in the joined haystack. */
function textMatch(hay: (string | null | undefined)[], q: string): boolean {
  const words = q.toLowerCase().split(/\s+/).filter(Boolean)
  if (words.length === 0) return true
  const s = hay.filter(Boolean).join(' ').toLowerCase()
  return words.every((w) => s.includes(w))
}

function pluginHay(p: RegistryPlugin): (string | null | undefined)[] {
  return [
    p.name,
    p.id,
    p.description,
    p.repository_label,
    p.category,
    ...(p.tags ?? []),
    ...p.hooks,
  ]
}

function mcpHay(m: RegistryMcpServer): (string | null | undefined)[] {
  return [
    m.name,
    m.id,
    m.description,
    m.author,
    m.category,
    m.transport,
    m.command,
    m.url,
    ...(m.tags ?? []),
  ]
}

/** New editor draft prefilled from a registry MCP template. */
function draftFromTemplate(m: RegistryMcpServer): McpServer {
  return {
    id: crypto.randomUUID(),
    name: m.id.replace(/[^A-Za-z0-9_-]/g, '-').slice(0, 64),
    transport: m.transport,
    command: m.command ?? '',
    args: [...(m.args ?? [])],
    env: (m.env ?? []).map((kv) => ({ ...kv })),
    url: m.url ?? '',
    headers: (m.headers ?? []).map((kv) => ({ ...kv })),
    enabled: true,
    providers: [],
    disabled_tools: [],
  }
}

/** Clickable category + tag chips shown on a card; clicking narrows the filters. */
function CardChips({
  tags,
  category,
  activeTags,
  onTag,
  onCategory,
}: {
  tags?: string[]
  category?: string | null
  activeTags: Set<string>
  onTag: (t: string) => void
  onCategory: (c: string) => void
}) {
  if ((!tags || tags.length === 0) && !category) return null
  return (
    <div className="registry-tag-chips">
      {category && (
        <button
          type="button"
          className="mcp-chip registry-cat-chip"
          title={`Filter by category '${category}'`}
          onClick={() => onCategory(category)}
        >
          {category}
        </button>
      )}
      {(tags ?? []).map((t) => (
        <button
          key={t}
          type="button"
          className={`mcp-chip mcp-chip--toggle${activeTags.has(t) ? ' mcp-chip--on' : ''}`}
          title={`Filter by tag '${t}'`}
          data-testid={`registry-card-tag-${t}`}
          onClick={() => onTag(t)}
        >
          #{t}
        </button>
      ))}
    </div>
  )
}

/**
 * The action control for one registry row: Install / Upgrade / Installed, or a
 * disabled "needs a newer Peckboard" state when the entry's `min_peckboard`
 * floor isn't met. The test id is stable across states (`registry-install-…`);
 * `data-action` distinguishes them for assertions and styling.
 */
function renderAction(p: RegistryPlugin, isBusy: boolean, install: (p: RegistryPlugin) => void) {
  const compatible = p.compatible !== false
  const testid = `registry-install-${p.id}`
  const needs = `Requires Peckboard ≥ ${p.min_peckboard ?? '?'}`

  // Newer compatible version on offer → Upgrade.
  if (p.installed && p.upgrade_available && compatible) {
    return (
      <button
        type="button"
        className="plugin-approval-approve"
        data-testid={testid}
        data-action="upgrade"
        disabled={isBusy}
        onClick={() => install(p)}
      >
        {isBusy ? 'Upgrading…' : `Upgrade to v${p.version}`}
      </button>
    )
  }
  // Newer version exists but this Peckboard is too old for it.
  if (p.installed && p.upgrade_available && !compatible) {
    return (
      <button
        type="button"
        className="plugin-approval-deny"
        data-testid={testid}
        data-action="incompatible"
        disabled
        title={needs}
      >
        Update needs Peckboard ≥ {p.min_peckboard}
      </button>
    )
  }
  // Installed and current.
  if (p.installed) {
    return (
      <button
        type="button"
        className="plugin-approval-approve"
        data-testid={testid}
        data-action="installed"
        disabled
      >
        Installed
      </button>
    )
  }
  // Not installed and this Peckboard is too old.
  if (!compatible) {
    return (
      <button
        type="button"
        className="plugin-approval-approve"
        data-testid={testid}
        data-action="incompatible"
        disabled
        title={needs}
      >
        Needs Peckboard ≥ {p.min_peckboard}
      </button>
    )
  }
  // Not installed, compatible → Install.
  return (
    <button
      type="button"
      className="plugin-approval-approve"
      data-testid={testid}
      data-action="install"
      disabled={isBusy}
      onClick={() => install(p)}
    >
      {isBusy ? 'Installing…' : 'Install'}
    </button>
  )
}

function BrowseTab({
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
  const [kind, setKind] = useState<Kind>('all')
  const [category, setCategory] = useState('')
  const [selTags, setSelTags] = useState<Set<string>>(new Set())

  // The user's configured MCP servers — powers the "Added" state and the
  // save target of the add flow. Same endpoint Settings → MCP Servers uses.
  const [mcpConfig, setMcpConfig] = useState<{ servers: McpServer[]; supported: string[] }>({
    servers: [],
    supported: [],
  })
  const [adding, setAdding] = useState<RegistryMcpServer | null>(null)
  const [addError, setAddError] = useState<string | null>(null)
  const providers = useResourcesStore((s) => s.providers)
  const fetchModels = useResourcesStore((s) => s.fetchModels)
  useEffect(() => {
    if (providers.length === 0) fetchModels()
  }, [providers.length, fetchModels])
  const providerLabel = (id: string) =>
    providers.find((p) => p.id === id)?.display_name ?? id.charAt(0).toUpperCase() + id.slice(1)

  const loadMcp = useCallback(() => {
    authedFetch('/api/settings/mcp-servers')
      .then((res) => (res.ok ? res.json() : null))
      .then((d) => {
        if (d) {
          setMcpConfig({
            servers: Array.isArray(d.servers)
              ? d.servers.map((s: McpServer) => ({ ...s, disabled_tools: s.disabled_tools ?? [] }))
              : [],
            supported: Array.isArray(d.supported_providers) ? d.supported_providers : [],
          })
        }
      })
      .catch(() => {})
  }, [])
  useEffect(loadMcp, [loadMcp])

  const addedNames = useMemo(
    () => new Set(mcpConfig.servers.map((s) => s.name.toLowerCase())),
    [mcpConfig.servers],
  )

  const categories = useMemo(() => {
    const set = new Set<string>()
    for (const p of data.plugins) if (p.category) set.add(p.category)
    for (const m of data.mcp_servers) if (m.category) set.add(m.category)
    return [...set].sort()
  }, [data])

  const allTags = useMemo(() => {
    const set = new Set<string>()
    for (const p of data.plugins) for (const t of p.tags ?? []) set.add(t)
    for (const m of data.mcp_servers) for (const t of m.tags ?? []) set.add(t)
    return [...set].sort()
  }, [data])

  const toggleTag = (t: string) =>
    setSelTags((prev) => {
      const next = new Set(prev)
      if (next.has(t)) next.delete(t)
      else next.add(t)
      return next
    })

  const catOk = (c?: string | null) => !category || c === category
  const tagOk = (t?: string[]) => [...selTags].every((s) => (t ?? []).includes(s))

  const visiblePlugins =
    kind === 'mcp'
      ? []
      : data.plugins.filter(
          (p) => textMatch(pluginHay(p), query) && catOk(p.category) && tagOk(p.tags),
        )
  const visibleMcp =
    kind === 'plugins'
      ? []
      : data.mcp_servers.filter(
          (m) => textMatch(mcpHay(m), query) && catOk(m.category) && tagOk(m.tags),
        )

  const total = data.plugins.length + data.mcp_servers.length
  const shown = visiblePlugins.length + visibleMcp.length
  const filtersActive = query.trim() !== '' || kind !== 'all' || category !== '' || selTags.size > 0

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

  const saveMcp = async (draft: McpServer) => {
    setAddError(null)
    const next = [...mcpConfig.servers, tidy(draft)]
    try {
      const res = await authedFetch('/api/settings/mcp-servers', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ servers: next }),
      })
      if (!res.ok) {
        const d = await res.json().catch(() => null)
        setAddError(d?.error ?? `Save failed (${res.status}).`)
        return
      }
      setAdding(null)
      loadMcp()
    } catch {
      setAddError('Save failed — server unreachable.')
    }
  }

  return (
    <div className="registry-panel" data-testid="registry-plugins-tab">
      <div className="registry-controls">
        <input
          type="search"
          className="registry-search"
          data-testid="registry-search"
          placeholder="Search plugins & MCP servers…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
        <div className="registry-kind-chips" role="group" aria-label="Kind">
          {(['all', 'plugins', 'mcp'] as Kind[]).map((k) => (
            <button
              key={k}
              type="button"
              className={`mcp-chip mcp-chip--toggle${kind === k ? ' mcp-chip--on' : ''}`}
              data-testid={`registry-kind-${k}`}
              aria-pressed={kind === k}
              onClick={() => setKind(k)}
            >
              {k === 'all' ? 'All' : k === 'plugins' ? 'Plugins' : 'MCP servers'}
            </button>
          ))}
        </div>
        <select
          className="plugin-setting-select registry-category-select"
          data-testid="registry-filter-category"
          aria-label="Category"
          value={category}
          onChange={(e) => setCategory(e.target.value)}
        >
          <option value="">All categories</option>
          {categories.map((c) => (
            <option key={c} value={c}>
              {c}
            </option>
          ))}
        </select>
      </div>

      {allTags.length > 0 && (
        <div className="registry-tag-row" data-testid="registry-tag-row">
          {allTags.map((t) => (
            <button
              key={t}
              type="button"
              className={`mcp-chip mcp-chip--toggle${selTags.has(t) ? ' mcp-chip--on' : ''}`}
              data-testid={`registry-tag-${t}`}
              aria-pressed={selTags.has(t)}
              onClick={() => toggleTag(t)}
            >
              #{t}
            </button>
          ))}
        </div>
      )}

      <div className="registry-result-line">
        <span data-testid="registry-result-count">
          {shown} of {total} shown
        </span>
        {filtersActive && (
          <button
            type="button"
            className="mcp-btn registry-clear-filters"
            data-testid="registry-clear-filters"
            onClick={() => {
              setQuery('')
              setKind('all')
              setCategory('')
              setSelTags(new Set())
            }}
          >
            Clear filters
          </button>
        )}
      </div>

      {installError && <p className="plugin-card-error">{installError}</p>}
      {addError && <p className="plugin-card-error">{addError}</p>}

      {shown === 0 && (
        <p className="plugin-permissions-empty">
          {total === 0 ? 'Nothing available in the registry.' : 'Nothing matches your filters.'}
        </p>
      )}

      {visiblePlugins.length > 0 && (
        <>
          {visibleMcp.length > 0 && <div className="registry-group-title">Plugins</div>}
          <ul className="registry-grid">
            {visiblePlugins.map((p) => (
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
                  <span className="registry-kind-badge">Plugin</span>
                  {p.upgrade_available ? (
                    <span
                      className="plugin-badge plugin-badge--pending"
                      data-testid={`registry-update-badge-${p.id}`}
                    >
                      Update available
                    </span>
                  ) : p.installed ? (
                    <span className="plugin-badge plugin-badge--approved">Installed</span>
                  ) : null}
                </div>
                <div className="registry-plugin-source">
                  {p.repository_label}
                  {p.installed && p.installed_version && p.installed_version !== p.version && (
                    <> · installed v{p.installed_version}</>
                  )}
                </div>
                <p className="plugin-card-description">{p.description}</p>
                <CardChips
                  tags={p.tags}
                  category={p.category}
                  activeTags={selTags}
                  onTag={toggleTag}
                  onCategory={setCategory}
                />
                <HookList hooks={p.hooks} title="Hooks" />
                <div className="wasm-plugin-actions">{renderAction(p, busy === p.id, install)}</div>
              </li>
            ))}
          </ul>
        </>
      )}

      {visibleMcp.length > 0 && (
        <>
          {visiblePlugins.length > 0 && <div className="registry-group-title">MCP servers</div>}
          <ul className="registry-grid">
            {visibleMcp.map((m) => {
              const added = addedNames.has(m.id.toLowerCase())
              const compatible = m.compatible !== false
              return (
                <li
                  key={`${m.repository}:${m.id}`}
                  className="wasm-plugin-row"
                  data-testid={`registry-mcp-${m.id}`}
                  data-added={added}
                >
                  <div className="wasm-plugin-head">
                    <span className="wasm-plugin-name">{m.name}</span>
                    <span className={`mcp-badge mcp-badge--${m.transport}`}>
                      {m.transport === 'stdio' ? 'stdio' : m.transport.toUpperCase()}
                    </span>
                    <span className="registry-kind-badge registry-kind-badge--mcp">MCP server</span>
                    {added && <span className="plugin-badge plugin-badge--approved">Added</span>}
                  </div>
                  <div className="registry-plugin-source">
                    {m.author}
                    {m.homepage && (
                      <>
                        {' · '}
                        <a href={m.homepage} target="_blank" rel="noreferrer">
                          docs
                        </a>
                      </>
                    )}
                    {' · '}
                    {m.repository_label}
                  </div>
                  <p className="plugin-card-description">{m.description}</p>
                  {m.setup_note && <p className="registry-setup-note">{m.setup_note}</p>}
                  <CardChips
                    tags={m.tags}
                    category={m.category}
                    activeTags={selTags}
                    onTag={toggleTag}
                    onCategory={setCategory}
                  />
                  <div className="wasm-plugin-actions">
                    {!compatible ? (
                      <button
                        type="button"
                        className="plugin-approval-approve"
                        data-testid={`registry-add-mcp-${m.id}`}
                        data-action="incompatible"
                        disabled
                        title={`Requires Peckboard ≥ ${m.min_peckboard ?? '?'}`}
                      >
                        Needs Peckboard ≥ {m.min_peckboard}
                      </button>
                    ) : added ? (
                      <button
                        type="button"
                        className="plugin-approval-approve"
                        data-testid={`registry-add-mcp-${m.id}`}
                        data-action="added"
                        disabled
                      >
                        Added
                      </button>
                    ) : (
                      <button
                        type="button"
                        className="plugin-approval-approve"
                        data-testid={`registry-add-mcp-${m.id}`}
                        data-action="add"
                        onClick={() => {
                          setAddError(null)
                          setAdding(m)
                        }}
                      >
                        Add to MCP Servers
                      </button>
                    )}
                  </div>
                </li>
              )
            })}
          </ul>
        </>
      )}

      {adding && (
        <ServerModal
          draft={draftFromTemplate(adding)}
          isNew
          others={mcpConfig.servers}
          supported={mcpConfig.supported}
          providerLabel={providerLabel}
          note={adding.setup_note ?? undefined}
          onCancel={() => setAdding(null)}
          onSave={saveMcp}
        />
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
