import { authedFetch } from '../store/auth'

/** A loaded WASM plugin and its approval status, from `/api/plugins`. */
export interface WasmPlugin {
  name: string
  hooks: string[]
  status: 'pending' | 'approved' | 'denied' | 'init_failed'
  error?: string | null
}

/**
 * Short, operator-facing gloss for each hook a plugin can request. Falls
 * back to a generic label for any hook not listed here, so a newly-added
 * hook is still shown to the operator (never silently hidden) before it
 * earns a description.
 */
export const HOOK_DESCRIPTIONS: Record<string, string> = {
  'http.request.before': 'Serve public HTTP requests under /plugin-api/*',
  'card.create.before': 'Inspect or veto cards as they are created',
  'card.update.before': 'Inspect or veto card updates',
  'card.priorities.list': 'Provide the list of card priorities',
  'session.reference.resolve': 'Resolve @-references in session messages',
  'mcp.tool.call.before': 'Inspect or veto MCP tool calls',
  'mcp.tool.call.after': 'Observe MCP tool-call results',
  'mcp.tool.call.failed': 'Observe failed MCP tool calls',
  'mcp.token.issue.before': 'Inspect or veto MCP token issuance',
  'mcp.token.issue.after': 'Observe issued MCP tokens',
  'mcp.token.revoke.after': 'Observe revoked MCP tokens',
  'mcp.config.write.before': 'Inspect or veto MCP config writes',
  'mcp.config.write.after': 'Observe MCP config writes',
  'mcp.config.delete.after': 'Observe MCP config deletions',
  todo: 'Receive todo-list updates',
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
  /** Resolved URL of the repository this entry came from. */
  repository: string
  /** Operator-facing label of that repository (slug or URL). */
  repository_label: string
  /** Whether a plugin with this id is already loaded in this instance. */
  installed: boolean
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
}

/** Fetch the aggregated registry across all repositories. */
export async function fetchRegistry(): Promise<RegistryData> {
  const res = await authedFetch('/api/plugins/registry')
  if (!res.ok) {
    const body = (await res.json().catch(() => null)) as { error?: string } | null
    throw new Error(body?.error ?? `HTTP ${res.status}`)
  }
  const data = (await res.json()) as Partial<RegistryData>
  return { repositories: data.repositories ?? [], plugins: data.plugins ?? [] }
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
