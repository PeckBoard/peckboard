import { authedFetch } from '../store/auth'

/** A loaded WASM plugin and its approval status, from `/api/plugins`. */
export interface WasmPlugin {
  name: string
  /** Required identity metadata the plugin declares in its manifest. */
  description: string
  version: string
  repository: string
  hooks: string[]
  /** Host permissions the plugin requests — approved alongside its hooks. */
  permissions: string[]
  status: 'pending' | 'approved' | 'denied' | 'init_failed'
  /** Manifest-declared settings schema (may be empty) — same shape as a
   *  built-in plugin's, rendered by the shared settings form. */
  settings_schema?: { fields: { key: string }[] }
  error?: string | null
}

/**
 * Operator-facing title + description for each host permission a plugin can
 * request, mirroring the hook presentation. Use {@link permissionMeta}, which
 * falls back to the raw id for any permission not listed here so a new one is
 * still surfaced (never silently hidden).
 */
export const PERMISSION_META: Record<string, HookMeta> = {
  provide_mcp_tools: {
    title: 'Provide MCP tools',
    description: 'Adds its own tools to the worker MCP server',
  },
  contribute_sidebar: {
    title: 'Add sidebar items',
    description: 'Contributes buttons to the left navigation rail',
  },
  session_read: {
    title: 'Read sessions',
    description: 'Reads sessions and their plugin metadata',
  },
  session_write: {
    title: 'Write sessions',
    description: 'Creates sessions and writes their plugin metadata',
  },
  session_dispatch: {
    title: 'Dispatch sessions',
    description: 'Resumes sessions and spawns agent runs',
  },
  event_append: {
    title: 'Append events',
    description: 'Persists events onto sessions',
  },
  data_store: {
    title: 'Store plugin data',
    description: 'Keeps its own durable documents in Peckboard',
  },
  broadcast: {
    title: 'Broadcast updates',
    description: 'Pushes live updates to connected clients',
  },
  project_files_read: {
    title: 'Read project files',
    description: 'Reads files within a project to analyze the codebase',
  },
}

/** Resolve a permission id to its operator-facing title + description. */
export function permissionMeta(permission: string): HookMeta {
  return PERMISSION_META[permission] ?? { title: permission, description: 'Custom permission' }
}

/** Operator-facing title + description for one hook a plugin can request. */
export interface HookMeta {
  /** Short, human-readable name shown as the row's bold title. */
  title: string
  /** One-line gloss shown beneath the title. */
  description: string
}

/**
 * Operator-facing title + description for each hook a plugin can request,
 * presented the same way as permissions (a bold title over a muted
 * description). Use {@link hookMeta} rather than indexing this directly:
 * it falls back to the raw hook id as the title for any hook not listed
 * here, so a newly-added hook is still shown to the operator (never
 * silently hidden) before it earns a description.
 */
export const HOOK_META: Record<string, HookMeta> = {
  'http.request.before': {
    title: 'Serve HTTP requests',
    description: 'Serves public HTTP requests under /plugin-api/*',
  },
  'card.create.before': {
    title: 'Inspect new cards',
    description: 'Inspects or vetoes cards as they are created',
  },
  'card.update.before': {
    title: 'Inspect card updates',
    description: 'Inspects or vetoes card updates',
  },
  'card.priorities.list': {
    title: 'Provide card priorities',
    description: 'Provides the list of card priorities',
  },
  'session.reference.resolve': {
    title: 'Resolve references',
    description: 'Resolves @-references in session messages',
  },
  'session.message.before': {
    title: 'Pre-process chat messages',
    description: 'Intercepts or rewrites chat messages before they reach the agent',
  },
  'mcp.tool.call.before': {
    title: 'Gate MCP tool calls',
    description: 'Inspects or vetoes MCP tool calls',
  },
  'mcp.tool.call.after': {
    title: 'Observe MCP tool results',
    description: 'Observes MCP tool-call results',
  },
  'mcp.tool.call.failed': {
    title: 'Observe MCP tool failures',
    description: 'Observes failed MCP tool calls',
  },
  'mcp.token.issue.before': {
    title: 'Gate MCP token issuance',
    description: 'Inspects or vetoes MCP token issuance',
  },
  'mcp.token.issue.after': {
    title: 'Observe issued MCP tokens',
    description: 'Observes issued MCP tokens',
  },
  'mcp.token.revoke.after': {
    title: 'Observe MCP token revocations',
    description: 'Observes revoked MCP tokens',
  },
  'mcp.config.write.before': {
    title: 'Gate MCP config writes',
    description: 'Inspects or vetoes MCP config writes',
  },
  'mcp.config.write.after': {
    title: 'Observe MCP config writes',
    description: 'Observes MCP config writes',
  },
  'mcp.config.delete.after': {
    title: 'Observe MCP config deletions',
    description: 'Observes MCP config deletions',
  },
  todo: {
    title: 'Receive todo updates',
    description: 'Receives todo-list updates',
  },
}

/**
 * Resolve a hook id to its operator-facing {@link HookMeta}. Unknown hooks
 * fall back to the raw id as the title with a generic description, so they
 * are still surfaced rather than hidden.
 */
export function hookMeta(hook: string): HookMeta {
  return HOOK_META[hook] ?? { title: hook, description: 'Custom hook' }
}

/** POST an approve/deny decision for one plugin's declared hook set. */
export function decidePluginApproval(
  pluginId: string,
  decision: 'approve' | 'deny',
): Promise<Response> {
  return authedFetch(`/api/plugins/${encodeURIComponent(pluginId)}/approval`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ decision }),
  })
}

/**
 * Uninstall an installed WASM plugin by id: shuts it down, deletes its
 * `.wasm`, and clears its stored approval + settings server-side. Only
 * installed plugins can be removed — built-in plugins have no such route.
 */
export function uninstallPlugin(pluginId: string): Promise<Response> {
  return authedFetch(`/api/plugins/${encodeURIComponent(pluginId)}`, {
    method: 'DELETE',
  })
}

/** A plugin available in the registry, aggregated across repositories. */
export interface RegistryPlugin {
  id: string
  name: string
  description: string
  author: string
  homepage?: string | null
  version: string
  hooks: string[]
  /** Freeform discovery tags from the registry entry. */
  tags?: string[]
  /** Curated category (e.g. dev-tools, infrastructure). */
  category?: string | null
  /** Resolved URL of the repository this entry came from. */
  repository: string
  /** Operator-facing label of that repository (slug or URL). */
  repository_label: string
  /** Whether a plugin with this id is already loaded in this instance. */
  installed: boolean
  /** Version of the loaded plugin, when installed (for the upgrade delta). */
  installed_version?: string | null
  /** Minimum Peckboard version this entry declares, if any. */
  min_peckboard?: string | null
  /** Whether the running Peckboard satisfies `min_peckboard`. */
  compatible?: boolean
  /** Installed AND the registry version is strictly newer than what's loaded. */
  upgrade_available?: boolean
}

/** An MCP server template from the registry (one-click add — nothing is downloaded). */
export interface RegistryMcpServer {
  id: string
  name: string
  description: string
  author: string
  homepage?: string | null
  transport: 'stdio' | 'http' | 'sse'
  command: string
  args: string[]
  env: { key: string; value: string }[]
  url: string
  headers: { key: string; value: string }[]
  setup_note?: string | null
  /** Human install steps for the host binary (stdio transport). */
  install?: string[]
  tags?: string[]
  category?: string | null
  repository: string
  repository_label: string
  min_peckboard?: string | null
  compatible?: boolean
}
/** One configured registry repository plus its reachability this fetch. */
export interface RegistryRepo {
  url: string
  label: string
  /** Whether this repository can be removed (the env override can't). */
  removable: boolean
  /** Whether its registry.json fetched successfully. */
  ok: boolean
  error?: string
}

/** The aggregate registry response powering both tabs. */
export interface RegistryData {
  repositories: RegistryRepo[]
  plugins: RegistryPlugin[]
  mcp_servers: RegistryMcpServer[]
  /** The running Peckboard version, for "needs Peckboard ≥ X" messaging. */
  peckboard_version?: string
}

/** Fetch the aggregated registry across all repositories. */
export async function fetchRegistry(): Promise<RegistryData> {
  const res = await authedFetch('/api/plugins/registry')
  if (!res.ok) {
    const body = (await res.json().catch(() => null)) as { error?: string } | null
    throw new Error(body?.error ?? `HTTP ${res.status}`)
  }
  const data = (await res.json()) as Partial<RegistryData>
  return {
    repositories: data.repositories ?? [],
    plugins: data.plugins ?? [],
    mcp_servers: data.mcp_servers ?? [],
    peckboard_version: data.peckboard_version,
  }
}

/** Add a registry repository (an `owner/repo` slug or an https URL). */
export function addRepository(repository: string): Promise<Response> {
  return authedFetch('/api/plugins/repositories', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ repository }),
  })
}

/** Remove a registry repository by its resolved URL. */
export function removeRepository(url: string): Promise<Response> {
  return authedFetch('/api/plugins/repositories', {
    method: 'DELETE',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ url }),
  })
}

/**
 * Install a registry plugin by id from a specific repository (downloads +
 * SHA-256-verifies server-side, then loads it inert).
 */
export function installRegistryPlugin(id: string, repository: string): Promise<Response> {
  return authedFetch('/api/plugins/registry/install', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ id, repository }),
  })
}
