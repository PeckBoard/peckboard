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
