import { useEffect, useMemo, useState } from 'react'
import { authedFetch } from '../store/auth'
import { useResourcesStore } from '../store/resources'
import Modal from './Modal'
import {
  probeMcpServer,
  tidy,
  type KvEntry,
  type McpServer,
  type McpTransport,
  type ProbeResult,
} from '../utils/mcpServers'

/**
 * Settings → MCP Servers: the editor for user-defined MCP servers.
 *
 * Servers are stored as one list (GET/PUT /api/settings/mcp-servers) and
 * injected into agent sessions at dispatch time alongside the built-in
 * `peckboard` server — Claude via the per-session `--mcp-config` file,
 * Cursor via the workspace `.cursor/mcp.json`. Providers without an
 * external-MCP hook (Grok, Ollama) are listed as unsupported.
 *
 * Client-side validation mirrors `service::mcp_server::user_servers` in
 * the backend; the server re-validates on PUT.
 */

const NAME_RE = /^[A-Za-z0-9_-]{1,64}$/

const TRANSPORT_LABELS: Record<McpTransport, string> = {
  stdio: 'stdio',
  http: 'HTTP',
  sse: 'SSE',
}

function emptyServer(): McpServer {
  return {
    id: crypto.randomUUID(),
    name: '',
    transport: 'stdio',
    command: '',
    args: [],
    env: [],
    url: '',
    headers: [],
    enabled: true,
    providers: [],
    disabled_tools: [],
  }
}

/** First problem with `s`, or null. Mirrors the backend's `validate`. */
function validateServer(s: McpServer, others: McpServer[]): string | null {
  if (!NAME_RE.test(s.name)) {
    return "Name must be 1-64 characters: letters, digits, '-' or '_'."
  }
  if (s.name.toLowerCase() === 'peckboard') {
    return "The name 'peckboard' is reserved for the built-in server."
  }
  if (others.some((o) => o.id !== s.id && o.name.toLowerCase() === s.name.toLowerCase())) {
    return `A server named '${s.name}' already exists.`
  }
  if (s.transport === 'stdio') {
    if (!s.command.trim()) return 'A command is required for stdio transport.'
  } else if (!/^https?:\/\//.test(s.url)) {
    return 'URL must start with http:// or https://.'
  }
  for (const kv of [...s.env, ...s.headers]) {
    if (!kv.key.trim() && kv.value.trim()) return 'Every env/header row needs a key.'
  }
  return null
}

/** One-line summary shown on the card: what runs / where it connects. */
function summary(s: McpServer): string {
  if (s.transport === 'stdio') return [s.command, ...s.args].join(' ')
  return s.url
}

/**
 * Parse a pasted `{"mcpServers": {...}}` snippet (the shape every MCP
 * server README ships) — or a bare name → entry map — into servers.
 */
function parseImport(text: string): { servers: McpServer[]; error: string | null } {
  let json: unknown
  try {
    json = JSON.parse(text)
  } catch {
    return { servers: [], error: 'Not valid JSON.' }
  }
  if (typeof json !== 'object' || json === null || Array.isArray(json)) {
    return { servers: [], error: 'Expected a JSON object.' }
  }
  const root = json as Record<string, unknown>
  const map = (root.mcpServers ?? root) as Record<string, unknown>
  if (typeof map !== 'object' || map === null || Array.isArray(map)) {
    return { servers: [], error: '"mcpServers" is not an object.' }
  }
  const servers: McpServer[] = []
  for (const [name, raw] of Object.entries(map)) {
    if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) {
      return { servers: [], error: `Entry '${name}' is not an object.` }
    }
    const entry = raw as Record<string, unknown>
    const command = typeof entry.command === 'string' ? entry.command : ''
    const url = typeof entry.url === 'string' ? entry.url : ''
    if (!command && !url) {
      return { servers: [], error: `Entry '${name}' has neither "command" nor "url".` }
    }
    const kvList = (obj: unknown): KvEntry[] =>
      typeof obj === 'object' && obj !== null && !Array.isArray(obj)
        ? Object.entries(obj as Record<string, unknown>).map(([key, value]) => ({
            key,
            value: typeof value === 'string' ? value : JSON.stringify(value),
          }))
        : []
    const transport: McpTransport = command ? 'stdio' : entry.type === 'sse' ? 'sse' : 'http'
    servers.push({
      id: crypto.randomUUID(),
      name,
      transport,
      command,
      args: Array.isArray(entry.args) ? entry.args.map(String) : [],
      env: kvList(entry.env),
      url,
      headers: kvList(entry.headers),
      enabled: true,
      providers: [],
      disabled_tools: [],
    })
  }
  if (servers.length === 0) return { servers: [], error: 'No server entries found.' }
  return { servers, error: null }
}

export default function McpServersSection() {
  const [servers, setServers] = useState<McpServer[]>([])
  const [supported, setSupported] = useState<string[]>([])
  const [loaded, setLoaded] = useState(false)
  const [saveError, setSaveError] = useState<string | null>(null)
  const [editing, setEditing] = useState<McpServer | null>(null)
  const [isNew, setIsNew] = useState(false)
  const [importOpen, setImportOpen] = useState(false)
  const [deleting, setDeleting] = useState<McpServer | null>(null)
  const providers = useResourcesStore((s) => s.providers)
  const fetchModels = useResourcesStore((s) => s.fetchModels)

  useEffect(() => {
    if (providers.length === 0) fetchModels()
  }, [providers.length, fetchModels])

  const load = () => {
    authedFetch('/api/settings/mcp-servers')
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (data) {
          setServers(
            Array.isArray(data.servers)
              ? data.servers.map((s: McpServer) => ({
                  ...s,
                  disabled_tools: s.disabled_tools ?? [],
                }))
              : [],
          )
          setSupported(Array.isArray(data.supported_providers) ? data.supported_providers : [])
        }
        setLoaded(true)
      })
      .catch(() => setLoaded(true))
  }
  useEffect(load, [])

  const providerLabel = (id: string) =>
    providers.find((p) => p.id === id)?.display_name ?? id.charAt(0).toUpperCase() + id.slice(1)

  const unsupportedNote = useMemo(() => {
    const names = providers
      .filter((p) => !supported.includes(p.id) && p.id !== 'mock')
      .map((p) => p.display_name)
    return names.length > 0 ? names.join(', ') : null
  }, [providers, supported])

  /** Optimistically apply `next`, persist, resync from the server on failure. */
  const persist = async (next: McpServer[]) => {
    const prev = servers
    setServers(next)
    setSaveError(null)
    try {
      const res = await authedFetch('/api/settings/mcp-servers', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ servers: next }),
      })
      if (!res.ok) {
        const data = await res.json().catch(() => null)
        setSaveError(data?.error ?? `Save failed (${res.status}).`)
        setServers(prev)
        return false
      }
      return true
    } catch {
      setSaveError('Save failed — server unreachable.')
      setServers(prev)
      return false
    }
  }

  const saveDraft = async (draft: McpServer) => {
    const clean = tidy(draft)
    const next = isNew ? [...servers, clean] : servers.map((s) => (s.id === clean.id ? clean : s))
    if (await persist(next)) setEditing(null)
  }

  return (
    <section className="settings-section" data-testid="mcp-servers-section">
      <h3>MCP Servers</h3>
      <p className="form-hint">
        External Model Context Protocol servers — tool providers like GitHub, Playwright, or your
        own — injected into agent sessions next to the built-in PeckBoard tools. Applies from each
        session&apos;s next message.
        {unsupportedNote && <> {unsupportedNote} sessions can&apos;t use external MCP servers.</>}
      </p>

      {saveError && (
        <div className="mcp-error" role="alert">
          {saveError}
        </div>
      )}

      {loaded && servers.length === 0 && (
        <div className="mcp-empty" data-testid="mcp-empty">
          <div className="mcp-empty-title">No MCP servers configured</div>
          <div className="mcp-empty-text">
            Add one by hand, or paste the <code>mcpServers</code> JSON snippet from any MCP
            server&apos;s README.
          </div>
        </div>
      )}

      <div className="mcp-server-list">
        {servers.map((s) => (
          <div
            key={s.id}
            className={`mcp-server-card${s.enabled ? '' : ' mcp-server-card--off'}`}
            data-testid={`mcp-server-card-${s.name}`}
          >
            <div className="mcp-server-row">
              <label className="mcp-switch" title={s.enabled ? 'Enabled' : 'Disabled'}>
                <input
                  type="checkbox"
                  checked={s.enabled}
                  onChange={(e) =>
                    persist(
                      servers.map((o) => (o.id === s.id ? { ...o, enabled: e.target.checked } : o)),
                    )
                  }
                />
                <span className="mcp-switch-slider" />
              </label>
              <span className="mcp-server-name">{s.name}</span>
              <span className={`mcp-badge mcp-badge--${s.transport}`}>
                {TRANSPORT_LABELS[s.transport]}
              </span>
              <span className="mcp-server-actions">
                <button
                  type="button"
                  className="mcp-btn"
                  onClick={() => {
                    setIsNew(false)
                    setEditing({ ...s })
                  }}
                >
                  Edit
                </button>
                <button
                  type="button"
                  className="mcp-btn mcp-btn--danger"
                  onClick={() => setDeleting(s)}
                >
                  Delete
                </button>
              </span>
            </div>
            <div className="mcp-server-summary" title={summary(s)}>
              {summary(s)}
            </div>
            <div className="mcp-server-providers">
              {(s.providers.length > 0 ? s.providers : supported).map((p) => (
                <span key={p} className="mcp-chip">
                  {providerLabel(p)}
                </span>
              ))}
            </div>
            <ToolsPanel
              server={s}
              onToggleTool={(tool, on) =>
                persist(
                  servers.map((o) =>
                    o.id === s.id
                      ? {
                          ...o,
                          disabled_tools: on
                            ? o.disabled_tools.filter((t) => t !== tool)
                            : [...o.disabled_tools, tool],
                        }
                      : o,
                  ),
                )
              }
            />
          </div>
        ))}
      </div>

      <div className="mcp-actions">
        <button
          type="button"
          className="mcp-btn mcp-btn--primary"
          data-testid="mcp-add-server"
          onClick={() => {
            setIsNew(true)
            setEditing(emptyServer())
          }}
        >
          + Add server
        </button>
        <button
          type="button"
          className="mcp-btn"
          data-testid="mcp-import-json"
          onClick={() => setImportOpen(true)}
        >
          Import JSON
        </button>
      </div>

      {editing && (
        <ServerModal
          draft={editing}
          isNew={isNew}
          others={servers}
          supported={supported}
          providerLabel={providerLabel}
          onCancel={() => setEditing(null)}
          onSave={saveDraft}
        />
      )}

      {importOpen && (
        <ImportModal
          existing={servers}
          onCancel={() => setImportOpen(false)}
          onImport={async (imported) => {
            if (await persist([...servers, ...imported])) setImportOpen(false)
          }}
        />
      )}

      {deleting && (
        <Modal onClose={() => setDeleting(null)} maxWidth={420} data-testid="mcp-delete-modal">
          <h3>Delete {deleting.name}?</h3>
          <p className="form-hint">
            Agent sessions stop seeing this server from their next message. Its configuration is not
            recoverable.
          </p>
          <div className="mcp-modal-actions">
            <button type="button" className="mcp-btn" onClick={() => setDeleting(null)}>
              Cancel
            </button>
            <button
              type="button"
              className="mcp-btn mcp-btn--danger"
              data-testid="mcp-delete-confirm"
              onClick={async () => {
                if (await persist(servers.filter((s) => s.id !== deleting.id))) setDeleting(null)
              }}
            >
              Delete
            </button>
          </div>
        </Modal>
      )}
    </section>
  )
}

/**
 * Expandable per-server Tools panel: probes the server live (`tools/list`)
 * and lets the user switch individual tools off. Switches are enforced for
 * Claude (hard `--disallowedTools`) and Ollama (native client filter);
 * Cursor and Grok sessions load every advertised tool.
 */
function ToolsPanel({
  server,
  onToggleTool,
}: {
  server: McpServer
  onToggleTool: (tool: string, enabled: boolean) => void
}) {
  const [open, setOpen] = useState(false)
  const [probing, setProbing] = useState(false)
  const [probe, setProbe] = useState<ProbeResult | null>(null)

  const runProbe = async () => {
    setProbing(true)
    setProbe(null)
    setProbe(await probeMcpServer(server))
    setProbing(false)
  }

  const offCount = server.disabled_tools.length
  const disabled = new Set(server.disabled_tools)
  // Tools switched off but no longer advertised still render, so they can be
  // switched back on after a server-side rename/removal.
  const stale =
    probe && probe.ok
      ? server.disabled_tools.filter((t) => !probe.tools.some((pt) => pt.name === t))
      : []

  return (
    <div className="mcp-tools-panel">
      <button
        type="button"
        className="mcp-tools-toggle"
        aria-expanded={open}
        data-testid={`mcp-tools-toggle-${server.name}`}
        onClick={() => {
          const next = !open
          setOpen(next)
          if (next && probe === null && !probing) void runProbe()
        }}
      >
        {open ? '▾' : '▸'} Tools
        {offCount > 0 && <span className="mcp-tools-off-count">{offCount} off</span>}
      </button>
      {open && (
        <div className="mcp-tools-body" data-testid={`mcp-tools-body-${server.name}`}>
          {probing && <div className="mcp-tools-status">Connecting…</div>}
          {!probing && probe && !probe.ok && (
            <div className="mcp-error" data-testid={`mcp-tools-error-${server.name}`}>
              {probe.error}
            </div>
          )}
          {!probing && probe && probe.ok && probe.tools.length === 0 && stale.length === 0 && (
            <div className="mcp-tools-status">The server advertises no tools.</div>
          )}
          {!probing && probe && probe.ok && (
            <>
              {probe.tools.map((t) => (
                <label key={t.name} className="mcp-tool-row" title={t.description}>
                  <input
                    type="checkbox"
                    checked={!disabled.has(t.name)}
                    data-testid={`mcp-tool-toggle-${server.name}-${t.name}`}
                    onChange={(e) => onToggleTool(t.name, e.target.checked)}
                  />
                  <span className="mcp-tool-name">{t.name}</span>
                  {t.description && <span className="mcp-tool-desc">{t.description}</span>}
                </label>
              ))}
              {stale.map((t) => (
                <label key={t} className="mcp-tool-row mcp-tool-row--stale">
                  <input
                    type="checkbox"
                    checked={false}
                    data-testid={`mcp-tool-toggle-${server.name}-${t}`}
                    onChange={() => onToggleTool(t, true)}
                  />
                  <span className="mcp-tool-name">{t}</span>
                  <span className="mcp-tool-desc">no longer advertised by the server</span>
                </label>
              ))}
              <div className="mcp-tools-hint">
                Switched-off tools are enforced for Claude and Ollama sessions; Cursor and Grok load
                every advertised tool.
              </div>
            </>
          )}
          {!probing && (
            <button
              type="button"
              className="mcp-btn mcp-tools-refresh"
              data-testid={`mcp-tools-refresh-${server.name}`}
              onClick={() => void runProbe()}
            >
              Refresh
            </button>
          )}
        </div>
      )}
    </div>
  )
}

export function ServerModal({
  draft: initial,
  isNew,
  others,
  supported,
  providerLabel,
  onCancel,
  onSave,
  note,
}: {
  draft: McpServer
  isNew: boolean
  others: McpServer[]
  supported: string[]
  providerLabel: (id: string) => string
  onCancel: () => void
  onSave: (draft: McpServer) => void
  /** Optional hint under the title (e.g. a registry template's setup note). */
  note?: string
}) {
  const [draft, setDraft] = useState<McpServer>(initial)
  const [testing, setTesting] = useState(false)
  const [testResult, setTestResult] = useState<ProbeResult | null>(null)
  const [touched, setTouched] = useState(false)
  // Empty `providers` means "all supported" — the chips show that as
  // everything checked; checking all of them stores [] again.
  const checked = draft.providers.length > 0 ? draft.providers : supported
  const error = validateServer(tidy(draft), others)
  const noProvider = checked.length === 0

  const set = (patch: Partial<McpServer>) => setDraft((d) => ({ ...d, ...patch }))

  const toggleProvider = (id: string) => {
    const next = checked.includes(id) ? checked.filter((p) => p !== id) : [...checked, id]
    set({ providers: next.length === supported.length ? [] : next })
  }

  const rows = (
    list: KvEntry[],
    onChange: (next: KvEntry[]) => void,
    keyPlaceholder: string,
    valuePlaceholder: string,
    addLabel: string,
  ) => (
    <>
      {list.map((kv, i) => (
        <div key={i} className="plugin-setting-kv-row">
          <input
            type="text"
            placeholder={keyPlaceholder}
            value={kv.key}
            onChange={(e) => {
              const next = [...list]
              next[i] = { ...next[i], key: e.target.value }
              onChange(next)
            }}
          />
          <input
            type="text"
            placeholder={valuePlaceholder}
            value={kv.value}
            onChange={(e) => {
              const next = [...list]
              next[i] = { ...next[i], value: e.target.value }
              onChange(next)
            }}
          />
          <button
            type="button"
            className="plugin-setting-kv-remove"
            onClick={() => onChange(list.filter((_, idx) => idx !== i))}
          >
            Remove
          </button>
        </div>
      ))}
      <button
        type="button"
        className="plugin-setting-kv-add"
        onClick={() => onChange([...list, { key: '', value: '' }])}
      >
        {addLabel}
      </button>
    </>
  )

  return (
    <Modal onClose={onCancel} maxWidth={620} className="mcp-modal" data-testid="mcp-server-modal">
      <h3>{isNew ? 'Add MCP server' : `Edit ${initial.name}`}</h3>
      {note && <p className="form-hint">{note}</p>}
      <div className="plugin-settings">
        <label className="plugin-setting-field">
          <span className="plugin-setting-label">Name</span>
          <span className="plugin-setting-desc">
            Key in the generated config — tools show up as{' '}
            <code>mcp__{draft.name || 'name'}__…</code>
          </span>
          <input
            className="plugin-setting-input"
            type="text"
            value={draft.name}
            placeholder="github"
            data-testid="mcp-field-name"
            onChange={(e) => {
              setTouched(true)
              set({ name: e.target.value })
            }}
          />
        </label>

        <label className="plugin-setting-field">
          <span className="plugin-setting-label">Transport</span>
          <select
            className="plugin-setting-select"
            value={draft.transport}
            data-testid="mcp-field-transport"
            onChange={(e) => set({ transport: e.target.value as McpTransport })}
          >
            <option value="stdio">stdio — launch a local command</option>
            <option value="http">HTTP — streamable HTTP endpoint</option>
            <option value="sse">SSE — server-sent events endpoint</option>
          </select>
        </label>

        {draft.transport === 'stdio' ? (
          <>
            <label className="plugin-setting-field">
              <span className="plugin-setting-label">Command</span>
              <input
                className="plugin-setting-input mcp-mono"
                type="text"
                value={draft.command}
                placeholder="npx"
                data-testid="mcp-field-command"
                onChange={(e) => {
                  setTouched(true)
                  set({ command: e.target.value })
                }}
              />
            </label>
            <div className="plugin-setting-field">
              <span className="plugin-setting-label">Arguments</span>
              {draft.args.map((a, i) => (
                <div key={i} className="plugin-setting-kv-row">
                  <input
                    type="text"
                    className="mcp-mono"
                    placeholder="-y @modelcontextprotocol/server-github"
                    value={a}
                    onChange={(e) => {
                      const args = [...draft.args]
                      args[i] = e.target.value
                      set({ args })
                    }}
                  />
                  <button
                    type="button"
                    className="plugin-setting-kv-remove"
                    onClick={() => set({ args: draft.args.filter((_, idx) => idx !== i) })}
                  >
                    Remove
                  </button>
                </div>
              ))}
              <button
                type="button"
                className="plugin-setting-kv-add"
                onClick={() => set({ args: [...draft.args, ''] })}
              >
                + Add argument
              </button>
            </div>
            <div className="plugin-setting-field">
              <span className="plugin-setting-label">Environment variables</span>
              <span className="plugin-setting-desc">
                Stored as configured and passed to the command (e.g. API tokens).
              </span>
              {rows(draft.env, (env) => set({ env }), 'GITHUB_TOKEN', 'Value', '+ Add variable')}
            </div>
          </>
        ) : (
          <>
            <label className="plugin-setting-field">
              <span className="plugin-setting-label">URL</span>
              <input
                className="plugin-setting-input mcp-mono"
                type="url"
                value={draft.url}
                placeholder="https://example.com/mcp"
                data-testid="mcp-field-url"
                onChange={(e) => {
                  setTouched(true)
                  set({ url: e.target.value })
                }}
              />
            </label>
            <div className="plugin-setting-field">
              <span className="plugin-setting-label">Headers</span>
              {rows(
                draft.headers,
                (headers) => set({ headers }),
                'Authorization',
                'Bearer …',
                '+ Add header',
              )}
            </div>
          </>
        )}

        <div className="plugin-setting-field">
          <span className="plugin-setting-label">Providers</span>
          <span className="plugin-setting-desc">
            Which providers&apos; sessions get this server.
          </span>
          <div className="mcp-provider-chips">
            {supported.map((p) => (
              <button
                key={p}
                type="button"
                className={`mcp-chip mcp-chip--toggle${checked.includes(p) ? ' mcp-chip--on' : ''}`}
                data-testid={`mcp-provider-${p}`}
                aria-pressed={checked.includes(p)}
                onClick={() => toggleProvider(p)}
              >
                {providerLabel(p)}
              </button>
            ))}
          </div>
        </div>

        <label className="mcp-enabled-row">
          <input
            type="checkbox"
            checked={draft.enabled}
            onChange={(e) => set({ enabled: e.target.checked })}
          />
          <span className="plugin-setting-label">Enabled</span>
        </label>

        {touched && error && <div className="mcp-error">{error}</div>}
        {noProvider && <div className="mcp-error">Select at least one provider.</div>}
        <div className="mcp-test-row">
          <button
            type="button"
            className="mcp-btn"
            disabled={!!error || testing}
            data-testid="mcp-test-connection"
            onClick={async () => {
              setTesting(true)
              setTestResult(null)
              setTestResult(await probeMcpServer(tidy(draft)))
              setTesting(false)
            }}
          >
            {testing ? 'Testing…' : 'Test connection'}
          </button>
          {testResult &&
            (testResult.ok ? (
              <span className="mcp-test-ok" data-testid="mcp-test-result">
                ✓ {testResult.tools.length} tool{testResult.tools.length === 1 ? '' : 's'}
                {testResult.tools.length > 0 &&
                  `: ${testResult.tools
                    .slice(0, 6)
                    .map((t) => t.name)
                    .join(', ')}${testResult.tools.length > 6 ? ', …' : ''}`}
              </span>
            ) : (
              <span className="mcp-test-error" data-testid="mcp-test-result">
                {testResult.error}
              </span>
            ))}
        </div>

        <div className="mcp-modal-actions">
          <button type="button" className="mcp-btn" onClick={onCancel}>
            Cancel
          </button>
          <button
            type="button"
            className="mcp-btn mcp-btn--primary"
            disabled={!!error || noProvider}
            data-testid="mcp-server-save"
            onClick={() => onSave(draft)}
          >
            {isNew ? 'Add server' : 'Save changes'}
          </button>
        </div>
      </div>
    </Modal>
  )
}

function ImportModal({
  existing,
  onCancel,
  onImport,
}: {
  existing: McpServer[]
  onCancel: () => void
  onImport: (servers: McpServer[]) => void
}) {
  const [text, setText] = useState('')
  const parsed = useMemo(() => (text.trim() ? parseImport(text) : null), [text])
  const conflicts = useMemo(() => {
    if (!parsed || parsed.error) return []
    const names = new Set(existing.map((s) => s.name.toLowerCase()))
    return parsed.servers.filter((s) => names.has(s.name.toLowerCase())).map((s) => s.name)
  }, [parsed, existing])
  const invalid = useMemo(() => {
    if (!parsed || parsed.error) return null
    for (const s of parsed.servers) {
      const err = validateServer(tidy(s), existing)
      if (err) return `${s.name}: ${err}`
    }
    return null
  }, [parsed, existing])

  const problem =
    parsed?.error ??
    invalid ??
    (conflicts.length > 0 ? `Already configured: ${conflicts.join(', ')}.` : null)

  return (
    <Modal onClose={onCancel} maxWidth={620} className="mcp-modal" data-testid="mcp-import-modal">
      <h3>Import MCP servers</h3>
      <p className="form-hint">
        Paste the <code>{'{"mcpServers": {…}}'}</code> snippet from an MCP server&apos;s README —
        the same shape Claude Desktop and Cursor use. Imported servers apply to all supported
        providers; edit them afterwards to narrow that.
      </p>
      <textarea
        className="mcp-import-textarea"
        rows={10}
        spellCheck={false}
        placeholder={
          '{\n  "mcpServers": {\n    "github": {\n      "command": "npx",\n      "args": ["-y", "@modelcontextprotocol/server-github"],\n      "env": { "GITHUB_TOKEN": "…" }\n    }\n  }\n}'
        }
        value={text}
        data-testid="mcp-import-textarea"
        onChange={(e) => setText(e.target.value)}
      />
      {text.trim() && problem && <div className="mcp-error">{problem}</div>}
      {parsed && !problem && (
        <div className="mcp-import-preview">
          Ready to add: {parsed.servers.map((s) => s.name).join(', ')}
        </div>
      )}
      <div className="mcp-modal-actions">
        <button type="button" className="mcp-btn" onClick={onCancel}>
          Cancel
        </button>
        <button
          type="button"
          className="mcp-btn mcp-btn--primary"
          disabled={!parsed || !!problem}
          data-testid="mcp-import-confirm"
          onClick={() => parsed && onImport(parsed.servers)}
        >
          Import
        </button>
      </div>
    </Modal>
  )
}
