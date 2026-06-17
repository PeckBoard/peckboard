import { useCallback, useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import Modal from './Modal'
import HookList from './HookList'
import { decidePluginApproval, type WasmPlugin } from '../utils/pluginApproval'

/**
 * Startup approval prompt. A freshly-installed WASM plugin loads **inert**
 * — none of its hooks fire — until an operator approves the exact set of
 * hooks it declares. This modal surfaces every plugin still awaiting that
 * decision (one at a time), listing the hooks it asks for, and records the
 * operator's choice. The decision persists server-side, so an approved
 * plugin doesn't prompt again on the next restart (unless its hooks
 * change). Mounted only while authenticated; see `App.tsx`.
 */
export default function PluginApprovalPrompt() {
  const [pending, setPending] = useState<WasmPlugin[]>([])
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)

  const refresh = useCallback(() => {
    authedFetch('/api/plugins')
      .then((res) => (res.ok ? res.json() : Promise.reject(new Error(`HTTP ${res.status}`))))
      .then((data: { wasm_plugins?: WasmPlugin[] }) => {
        setPending((data.wasm_plugins ?? []).filter((p) => p.status === 'pending'))
      })
      .catch(() => {
        /* Leave the prompt closed if the catalog can't be read. */
      })
  }, [])

  useEffect(() => {
    refresh()
    // A decision in any tab broadcasts `plugin-approval`; refetch so this
    // prompt drops plugins others have already decided.
    const onEvent = () => refresh()
    window.addEventListener('peckboard:plugin-approval', onEvent)
    return () => window.removeEventListener('peckboard:plugin-approval', onEvent)
  }, [refresh])

  const decide = useCallback((pluginId: string, decision: 'approve' | 'deny') => {
    setBusy(pluginId)
    setError(null)
    decidePluginApproval(pluginId, decision)
      .then((res) => (res.ok ? res.json() : Promise.reject(new Error(`HTTP ${res.status}`))))
      .then(() => {
        // Drop the decided plugin right away; the ws event syncs others.
        setPending((prev) => prev.filter((p) => p.name !== pluginId))
      })
      .catch((e: Error) => setError(e.message))
      .finally(() => setBusy(null))
  }, [])

  if (pending.length === 0) return null
  // One plugin at a time — keeps each decision unambiguous.
  const plugin = pending[0]

  return (
    <Modal maxWidth={520} data-testid="plugin-approval-prompt">
      <div className="plugin-approval">
        <h3 className="plugin-approval-title">
          Approve plugin “<span data-testid="plugin-approval-name">{plugin.name}</span>”?
        </h3>
        <p className="plugin-approval-intro">
          This plugin is installed but inert. It is requesting permission to use the hooks below —
          nothing it declares runs until you approve.
        </p>
        <HookList hooks={plugin.hooks} testId="plugin-approval-hooks" title="Hooks" />
        {error && <p className="plugin-approval-error">{error}</p>}
        <div className="plugin-approval-actions">
          <button
            type="button"
            className="plugin-approval-deny"
            data-testid="plugin-approval-deny"
            disabled={busy === plugin.name}
            onClick={() => decide(plugin.name, 'deny')}
          >
            Deny
          </button>
          <button
            type="button"
            className="plugin-approval-approve"
            data-testid="plugin-approval-approve"
            disabled={busy === plugin.name}
            onClick={() => decide(plugin.name, 'approve')}
          >
            Approve
          </button>
        </div>
        {pending.length > 1 && (
          <p className="plugin-approval-more">{pending.length - 1} more awaiting review</p>
        )}
      </div>
    </Modal>
  )
}
