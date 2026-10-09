import { useState, type ReactNode } from 'react'
import TerminalPane from './TerminalPane'
import type { TerminalStatus } from '../../store/terminals'
import type { ViewTerminalMeta } from '../../store/views'
import { describeActionError } from '../../utils/actionError'
import './Terminal.css'

/**
 * A terminal inside a saved view's pane. Mounts xterm only while the pane is
 * on screen (a maximized neighbour or the narrow switcher hides it), takes
 * the keyboard only when the pane is focused, and refits through
 * `TerminalPane`'s ResizeObserver as dividers move. A closed terminal shows
 * a placeholder offering to reopen a shell on the same host.
 */
export default function ViewTerminalPane({
  terminalId,
  meta,
  focused,
  visible,
  onStatus,
  onReopen,
  replaceSlot,
}: {
  terminalId: string
  meta: ViewTerminalMeta | undefined
  focused: boolean
  visible: boolean
  onStatus: (s: TerminalStatus) => void
  onReopen: () => Promise<void>
  /** The pane's "Replace…" control, shown on the closed placeholder. */
  replaceSlot: ReactNode
}) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')

  if (meta?.closed) {
    return (
      <div className="split-empty-leaf" data-testid="view-terminal-closed">
        <p>Terminal closed{meta.name ? ` — ${meta.name}` : ''}</p>
        <div className="view-terminal-closed-actions">
          <button
            type="button"
            className="btn-primary btn-sm"
            disabled={busy}
            data-testid="view-terminal-reopen"
            onClick={() => {
              setBusy(true)
              setError('')
              onReopen()
                .catch((e: unknown) =>
                  setError(describeActionError(e, "Couldn't open a terminal.")),
                )
                .finally(() => setBusy(false))
            }}
          >
            {busy ? 'Opening…' : `Reopen on ${meta.host_label}`}
          </button>
          {replaceSlot}
        </div>
        {error && (
          <p className="form-error" role="alert">
            {error}
          </p>
        )}
      </div>
    )
  }
  if (!visible) return null
  return (
    <div
      className="view-terminal-pane"
      data-testid="view-terminal-pane"
      data-terminal-id={terminalId}
    >
      <TerminalPane terminalId={terminalId} active={focused} onStatus={onStatus} />
    </div>
  )
}
