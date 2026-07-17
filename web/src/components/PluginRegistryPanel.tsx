import { useCallback, useEffect, useMemo, useState } from 'react'
import HookList from './HookList'
import Modal from './Modal'
import { MenuButton } from './Dropdown'
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
/** What the details modal is showing, when open. */
type Detail = { kind: 'plugin'; plugin: RegistryPlugin } | { kind: 'mcp'; mcp: RegistryMcpServer }

/**
 * The Plugin Registry Settings sub-page. Two tabs: **Browse** — every WASM
 * plugin AND MCP server template aggregated across all configured
 * repositories — and **Repositories** — manage the registry sources. One
 * `/api/plugins/registry` call powers both tabs.
 *
 * Browse renders compact one-line rows (the same `.plugin-row` language as
 * Settings → Plugins); everything beyond name + one-line summary lives in a
 * per-entry details modal opened by clicking the row. Filters: one search
 * box over names/descriptions/tags/categories, kind chips, a category
 * select, and a searchable tag dropdown (no tag wall).
 *
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

/**
 * Category + tag chips inside the details modal; clicking one applies it as
 * a filter (the caller also closes the modal so the narrowed list shows).
 */
function DetailChips({
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
    <div className="registry-detail-chips">
      {category && (
        <button
          type="button"
          className="mcp-chip registry-cat-chip"
          title={`Filter by category '${category}'`}
          data-testid="registry-detail-cat"
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
          data-testid={`registry-detail-tag-${t}`}
          onClick={() => onTag(t)}
        >
          #{t}
        </button>
      ))}
    </div>
  )
}

/**
 * The action control for one registry plugin: Install / Upgrade / Installed,
 * or a disabled "needs a newer Peckboard" state when the entry's
 * `min_peckboard` floor isn't met. Rendered both on the row and in the
 * details modal — `testidPrefix` keeps the two testids distinct while
 * `data-action` distinguishes states for assertions and styling.
 */
function renderAction(
  p: RegistryPlugin,
  isBusy: boolean,
  install: (p: RegistryPlugin) => void,
  testidPrefix = 'registry-install-',
) {
  const compatible = p.compatible !== false
  const testid = `${testidPrefix}${p.id}`
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

/**
 * The action control for one MCP template: Add / Added, or the disabled
 * incompatible state. `label` lets the row say "Add" while the modal says
 * "Add to MCP Servers"; `testidPrefix` keeps the two testids distinct.
 */
function renderMcpAction(
  m: RegistryMcpServer,
  added: boolean,
  onAdd: () => void,
  label = 'Add',
  testidPrefix = 'registry-add-mcp-',
) {
  const testid = `${testidPrefix}${m.id}`
  if (m.compatible === false) {
    return (
      <button
        type="button"
        className="plugin-approval-approve"
        data-testid={testid}
        data-action="incompatible"
        disabled
        title={`Requires Peckboard ≥ ${m.min_peckboard ?? '?'}`}
      >
        Needs Peckboard ≥ {m.min_peckboard}
      </button>
    )
  }
  if (added) {
    return (
      <button
        type="button"
        className="plugin-approval-approve"
        data-testid={testid}
        data-action="added"
        disabled
      >
        Added
      </button>
    )
  }
  return (
    <button
      type="button"
      className="plugin-approval-approve"
      data-testid={testid}
      data-action="add"
      onClick={onAdd}
    >
      {label}
    </button>
  )
}

/**
 * Full details for one registry entry, opened by clicking its row. Carries
 * everything the compact rows deliberately omit: full description, meta
 * (author / source / docs / version delta), category + tag chips (click =
 * apply as filter), hooks for plugins, transport + config + setup note for
 * MCP templates, and the install/add action.
 */
function RegistryDetailModal({
  detail,
  busy,
  added,
  activeTags,
  onClose,
  onInstall,
  onAdd,
  onTag,
  onCategory,
}: {
  detail: Detail
  busy: string | null
  added: boolean
  activeTags: Set<string>
  onClose: () => void
  onInstall: (p: RegistryPlugin) => void
  onAdd: (m: RegistryMcpServer) => void
  onTag: (t: string) => void
  onCategory: (c: string) => void
}) {
  if (detail.kind === 'plugin') {
    const p = detail.plugin
    return (
      <Modal
        onClose={onClose}
        className="plugin-details-modal"
        maxWidth={560}
        data-testid="registry-detail-modal"
      >
        <header className="plugin-details-head">
          <h2>{p.name}</h2>
          <div className="registry-detail-badges">
            <span className="registry-kind-badge">Plugin</span>
            {p.upgrade_available ? (
              <span className="plugin-badge plugin-badge--pending">Update available</span>
            ) : p.installed ? (
              <span className="plugin-badge plugin-badge--approved">Installed</span>
            ) : null}
          </div>
        </header>
        <div className="plugin-card-meta">
          <span>v{p.version}</span>
          {p.installed && p.installed_version && p.installed_version !== p.version && (
            <>
              <span>·</span>
              <span>installed v{p.installed_version}</span>
            </>
          )}
          <span>·</span>
          <span>{p.author}</span>
          {p.homepage && (
            <>
              <span>·</span>
              <a href={p.homepage} target="_blank" rel="noreferrer">
                docs
              </a>
            </>
          )}
          <span>·</span>
          <span>{p.repository_label}</span>
        </div>
        <p className="plugin-card-description">{p.description}</p>
        <DetailChips
          tags={p.tags}
          category={p.category}
          activeTags={activeTags}
          onTag={onTag}
          onCategory={onCategory}
        />
        <HookList hooks={p.hooks} title="Hooks" />
        {p.compatible === false && (
          <p className="registry-setup-note">
            Requires Peckboard ≥ {p.min_peckboard ?? '?'} — update Peckboard to install this
            version.
          </p>
        )}
        <div className="form-actions">
          {renderAction(p, busy === p.id, onInstall, 'registry-modal-install-')}
          <button type="button" className="btn-secondary" onClick={onClose}>
            Close
          </button>
        </div>
      </Modal>
    )
  }

  const m = detail.mcp
  return (
    <Modal
      onClose={onClose}
      className="plugin-details-modal"
      maxWidth={560}
      data-testid="registry-detail-modal"
    >
      <header className="plugin-details-head">
        <h2>{m.name}</h2>
        <div className="registry-detail-badges">
          <span className="registry-kind-badge registry-kind-badge--mcp">MCP server</span>
          <span className={`mcp-badge mcp-badge--${m.transport}`}>
            {m.transport === 'stdio' ? 'stdio' : m.transport.toUpperCase()}
          </span>
          {added && <span className="plugin-badge plugin-badge--approved">Added</span>}
        </div>
      </header>
      <div className="plugin-card-meta">
        <span>{m.author}</span>
        {m.homepage && (
          <>
            <span>·</span>
            <a href={m.homepage} target="_blank" rel="noreferrer">
              docs
            </a>
          </>
        )}
        <span>·</span>
        <span>{m.repository_label}</span>
      </div>
      <p className="plugin-card-description">{m.description}</p>
      {(m.command || m.url) && (
        <div className="registry-detail-config">
          <div className="plugin-section-title">Configuration</div>
          <code className="registry-detail-cmd">
            {m.transport === 'stdio' ? [m.command, ...(m.args ?? [])].join(' ') : m.url}
          </code>
        </div>
      )}
      {m.setup_note && <p className="registry-setup-note">{m.setup_note}</p>}
      <DetailChips
        tags={m.tags}
        category={m.category}
        activeTags={activeTags}
        onTag={onTag}
        onCategory={onCategory}
      />
      {m.compatible === false && (
        <p className="registry-setup-note">Requires Peckboard ≥ {m.min_peckboard ?? '?'}.</p>
      )}
      <div className="form-actions">
        {renderMcpAction(m, added, () => onAdd(m), 'Add to MCP Servers', 'registry-modal-add-mcp-')}
        <button type="button" className="btn-secondary" onClick={onClose}>
          Close
        </button>
      </div>
    </Modal>
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
  const [detail, setDetail] = useState<Detail | null>(null)

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
      : data.plugins
          .filter((p) => textMatch(pluginHay(p), query) && catOk(p.category) && tagOk(p.tags))
          .sort((a, b) => a.name.localeCompare(b.name))
  const visibleMcp =
    kind === 'plugins'
      ? []
      : data.mcp_servers
          .filter((m) => textMatch(mcpHay(m), query) && catOk(m.category) && tagOk(m.tags))
          .sort((a, b) => a.name.localeCompare(b.name))

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

  /** Open the prefilled MCP editor (closes the details modal if open). */
  const beginAdd = (m: RegistryMcpServer) => {
    setAddError(null)
    setDetail(null)
    setAdding(m)
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

  const detailAdded = detail?.kind === 'mcp' ? addedNames.has(detail.mcp.id.toLowerCase()) : false

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
        {allTags.length > 0 && (
          <MenuButton
            ariaLabel="Filter by tag"
            triggerClassName="mcp-btn registry-tag-btn"
            testId="registry-tag-filter"
            searchable
            searchPlaceholder="Filter tags…"
            items={allTags.map((t) => ({
              label: `#${t}`,
              active: selTags.has(t),
              hint: selTags.has(t) ? '✓' : undefined,
              onSelect: () => toggleTag(t),
              testId: `registry-tag-${t}`,
            }))}
          >
            Tags{selTags.size > 0 ? ` · ${selTags.size}` : ''} ▾
          </MenuButton>
        )}
      </div>

      {selTags.size > 0 && (
        <div className="registry-active-tags" data-testid="registry-active-tags">
          {[...selTags].sort().map((t) => (
            <button
              key={t}
              type="button"
              className="mcp-chip mcp-chip--toggle mcp-chip--on"
              title={`Remove tag filter '${t}'`}
              data-testid={`registry-active-tag-${t}`}
              onClick={() => toggleTag(t)}
            >
              #{t} ✕
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
          <ul className="wasm-plugins-list registry-list">
            {visiblePlugins.map((p) => (
              <li
                key={`${p.repository}:${p.id}`}
                className="wasm-plugin-row plugin-row registry-row"
                data-testid={`registry-plugin-${p.id}`}
                data-installed={p.installed}
              >
                <button
                  type="button"
                  className="plugin-row-body"
                  data-testid={`registry-open-${p.id}`}
                  onClick={() => setDetail({ kind: 'plugin', plugin: p })}
                >
                  <span className="registry-kind-badge">Plugin</span>
                  <span className="wasm-plugin-name">
                    {p.name} <span className="wasm-plugin-version">v{p.version}</span>
                  </span>
                  {p.upgrade_available && (
                    <span
                      className="plugin-badge plugin-badge--pending"
                      data-testid={`registry-update-badge-${p.id}`}
                    >
                      Update available
                    </span>
                  )}
                  <span className="plugin-row-summary">{p.description}</span>
                </button>
                <div className="registry-row-action">{renderAction(p, busy === p.id, install)}</div>
              </li>
            ))}
          </ul>
        </>
      )}

      {visibleMcp.length > 0 && (
        <>
          {visiblePlugins.length > 0 && <div className="registry-group-title">MCP servers</div>}
          <ul className="wasm-plugins-list registry-list">
            {visibleMcp.map((m) => {
              const added = addedNames.has(m.id.toLowerCase())
              return (
                <li
                  key={`${m.repository}:${m.id}`}
                  className="wasm-plugin-row plugin-row registry-row"
                  data-testid={`registry-mcp-${m.id}`}
                  data-added={added}
                >
                  <button
                    type="button"
                    className="plugin-row-body"
                    data-testid={`registry-open-mcp-${m.id}`}
                    onClick={() => setDetail({ kind: 'mcp', mcp: m })}
                  >
                    <span className="registry-kind-badge registry-kind-badge--mcp">MCP</span>
                    <span className="wasm-plugin-name">{m.name}</span>
                    <span className="plugin-row-summary">{m.description}</span>
                  </button>
                  <div className="registry-row-action">
                    {renderMcpAction(m, added, () => beginAdd(m))}
                  </div>
                </li>
              )
            })}
          </ul>
        </>
      )}

      {detail && (
        <RegistryDetailModal
          detail={detail}
          busy={busy}
          added={detailAdded}
          activeTags={selTags}
          onClose={() => setDetail(null)}
          onInstall={(p) => {
            setDetail(null)
            install(p)
          }}
          onAdd={beginAdd}
          onTag={(t) => {
            toggleTag(t)
            setDetail(null)
          }}
          onCategory={(c) => {
            setCategory(c)
            setDetail(null)
          }}
        />
      )}

      {adding && (
        <ServerModal
          draft={draftFromTemplate(adding)}
          isNew
          others={mcpConfig.servers}
          supported={mcpConfig.supported}
          providerLabel={providerLabel}
          note={adding.setup_note ?? undefined}
          installSteps={adding.install}
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
      <ul className="wasm-plugins-list registry-list">
        {data.repositories.map((r: RegistryRepo, i: number) => (
          <li
            key={r.url}
            className="wasm-plugin-row plugin-row registry-row"
            data-testid={`registry-repo-${i}`}
            data-repo-url={r.url}
          >
            <div className="plugin-row-body registry-row-static">
              <span className="wasm-plugin-name">{r.label}</span>
              {r.ok ? (
                <span className="plugin-badge plugin-badge--approved">OK</span>
              ) : (
                <span className="plugin-badge plugin-badge--init_failed" title={r.error ?? ''}>
                  Unreachable
                </span>
              )}
              <span
                className="plugin-row-summary"
                title={!r.ok && r.error ? `${r.url} — ${r.error}` : r.url}
              >
                {!r.ok && r.error ? `${r.url} — ${r.error}` : r.url}
              </span>
            </div>
            <div className="registry-row-action">
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
