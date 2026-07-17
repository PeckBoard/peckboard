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

export interface McpServer {
  id: string
  name: string
  transport: McpTransport
  command: string
  args: string[]
  env: KvEntry[]
  url: string
  headers: KvEntry[]
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
