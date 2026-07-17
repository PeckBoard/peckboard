import { authedFetch } from '../store/auth'

/**
 * Shared model + helpers for user-defined MCP servers (Settings → MCP
 * Servers and the registry's one-click add). Mirrors the backend's
 * `service::mcp_server::user_servers`; kept out of the component files so
 * they only export components (react-refresh).
 */

/** One env-var or header row. A list, not a map, to preserve row order. */
export interface KvEntry {
  key: string
  value: string
}

export type McpTransport = 'stdio' | 'http' | 'sse'
/**
 * OAuth endpoint/client template for a server using sign-in (`auth:
 * 'oauth'`). Mirrors the backend's `McpOauthConfig` — every field optional;
 * missing pieces are discovered from the server's `.well-known` metadata.
 */
export interface McpOauthConfig {
  authorize_url?: string | null
  token_url?: string | null
  registration_url?: string | null
  client_id?: string | null
  client_secret?: string | null
  scopes?: string | null
  scope_param?: string | null
  token_field?: string | null
}

export interface McpServer {
  id: string
  name: string
  transport: McpTransport
  command: string
  args: string[]
  env: KvEntry[]
  url: string
  headers: KvEntry[]
  /** `''` = manual headers; `'oauth'` = provider sign-in via /api/mcp-oauth. */
  auth: string
  /** OAuth template (from the registry entry or user-entered credentials). */
  oauth?: McpOauthConfig | null
  enabled: boolean
  /** Provider ids this server applies to; empty = every supported provider. */
  providers: string[]
  /** Bare tool names switched off; enforced for Claude + Ollama sessions. */
  disabled_tools: string[]
}

/** Drop blank list rows the user left behind before saving. */
export function tidy(s: McpServer): McpServer {
  return {
    ...s,
    name: s.name.trim(),
    command: s.command.trim(),
    url: s.url.trim(),
    args: s.args.map((a) => a.trim()).filter((a) => a !== ''),
    env: s.env.filter((kv) => kv.key.trim() !== ''),
    headers: s.headers.filter((kv) => kv.key.trim() !== ''),
  }
}

export interface ProbeTool {
  name: string
  description: string
}

export type ProbeResult = { ok: true; tools: ProbeTool[] } | { ok: false; error: string }

/** POST the entry to the probe endpoint; network failures normalize to ok:false. */
export async function probeMcpServer(server: McpServer): Promise<ProbeResult> {
  try {
    const res = await authedFetch('/api/settings/mcp-servers/probe', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(server),
    })
    const data = await res.json().catch(() => null)
    if (!res.ok || !data) return { ok: false, error: `Probe failed (${res.status}).` }
    if (data.ok) return { ok: true, tools: Array.isArray(data.tools) ? data.tools : [] }
    return { ok: false, error: typeof data.error === 'string' ? data.error : 'Probe failed.' }
  } catch {
    return { ok: false, error: 'Probe failed — server unreachable.' }
  }
}

export interface CommandCheckResult {
  found: boolean
  resolved_path: string | null
  /** Human install steps (built-in per-runner hints from the server). */
  hints: string[]
  /** Suggested working folder for a one-off install session. */
  suggested_folder_path: string
}

/** Ask the server whether a stdio `command` exists on its PATH. */
export async function checkMcpCommand(command: string): Promise<CommandCheckResult | null> {
  try {
    const res = await authedFetch('/api/settings/mcp-servers/check-command', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ command }),
    })
    if (!res.ok) return null
    const data = await res.json().catch(() => null)
    if (!data || typeof data.found !== 'boolean') return null
    return {
      found: data.found,
      resolved_path: typeof data.resolved_path === 'string' ? data.resolved_path : null,
      hints: Array.isArray(data.hints) ? data.hints : [],
      suggested_folder_path:
        typeof data.suggested_folder_path === 'string' ? data.suggested_folder_path : '',
    }
  } catch {
    return null
  }
}

/** Connection status for one OAuth-connected server (token values stay server-side). */
export interface McpOauthTokenInfo {
  server_name: string
  connected: boolean
  expires_at_ms?: number | null
  obtained_at_ms?: number
  has_refresh_token?: boolean
}

export type StartOauthResult =
  | { ok: true; url: string }
  | { ok: false; needsClient: boolean; error: string; redirectUri?: string }

/**
 * Begin the OAuth sign-in for a server draft. `needsClient` means the
 * provider offers no dynamic client registration — the user must supply a
 * client id/secret from a provider-registered app (redirect URL included
 * for that registration).
 */
export async function startMcpOauth(server: McpServer): Promise<StartOauthResult> {
  try {
    const res = await authedFetch('/api/mcp-oauth/start', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ server }),
    })
    const data = (await res.json().catch(() => null)) as Record<string, unknown> | null
    if (res.ok && data && typeof data.url === 'string') return { ok: true, url: data.url }
    const needsClient = res.status === 422 && data?.error === 'needs_client'
    const error = needsClient
      ? typeof data?.message === 'string'
        ? data.message
        : 'This provider needs a client id and secret.'
      : typeof data?.error === 'string'
        ? data.error
        : `Sign-in could not start (${res.status}).`
    return {
      ok: false,
      needsClient,
      error,
      redirectUri: typeof data?.redirect_uri === 'string' ? data.redirect_uri : undefined,
    }
  } catch {
    return { ok: false, needsClient: false, error: 'Sign-in could not start — server unreachable.' }
  }
}

/** All connected servers' OAuth status, keyed by server id. */
export async function fetchMcpOauthTokens(): Promise<Record<string, McpOauthTokenInfo>> {
  try {
    const res = await authedFetch('/api/mcp-oauth/tokens')
    if (!res.ok) return {}
    const data = (await res.json().catch(() => null)) as {
      tokens?: Record<string, McpOauthTokenInfo>
    } | null
    return data?.tokens && typeof data.tokens === 'object' ? data.tokens : {}
  } catch {
    return {}
  }
}

/** Drop a server's stored OAuth token. */
export async function disconnectMcpOauth(serverId: string): Promise<boolean> {
  try {
    const res = await authedFetch(`/api/mcp-oauth/tokens/${encodeURIComponent(serverId)}`, {
      method: 'DELETE',
    })
    return res.ok
  } catch {
    return false
  }
}
