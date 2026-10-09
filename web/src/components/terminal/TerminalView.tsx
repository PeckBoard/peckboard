import { useState } from 'react'
import TerminalPane from './TerminalPane'
import {
  PHASE_LABEL,
  popOutTerminal,
  useTerminalsStore,
  type TerminalPhase,
  type TerminalStatus,
} from '../../store/terminals'
import './Terminal.css'

export function TerminalStatusPill({ phase }: { phase: TerminalPhase }) {
  return (
    <span className="terminal-status" data-phase={phase} data-testid="terminal-status">
      {PHASE_LABEL[phase]}
    </span>
  )
}

/** "This shell dies with the connection" — shown when the host has no tmux. */
export function NotPersistentBadge() {
  return (
    <span
      className="terminal-badge"
      title="tmux isn't installed on this host, so the shell ends if Peckboard restarts or the connection drops. Install tmux on the host to make it persistent."
      data-testid="terminal-not-persistent"
    >
      Not persistent across restarts
    </span>
  )
}

interface Props {
  terminalId: string
  active: boolean
}

/** A terminal tab: a one-line identity bar over the full-size shell. */
export default function TerminalView({ terminalId, active }: Props) {
  const info = useTerminalsStore((s) => s.terminals.find((t) => t.id === terminalId))
  const [status, setStatus] = useState<TerminalStatus | null>(null)
  const phase = status?.phase ?? info?.status.phase ?? 'connecting'
  const persistent = status?.persistent ?? info?.status.persistent ?? null

  return (
    <div
      className="terminal-view"
      data-testid="terminal-view"
      data-terminal-id={terminalId}
      style={{ display: active ? undefined : 'none' }}
    >
      <div className="terminal-bar">
        <span className="terminal-bar-name">{info?.name ?? 'Terminal'}</span>
        {info && <span className="terminal-bar-host">{info.host_label}</span>}
        <TerminalStatusPill phase={phase} />
        {persistent === false && <NotPersistentBadge />}
        <span className="terminal-bar-spacer" />
        <button
          type="button"
          className="terminal-bar-btn"
          onClick={() => popOutTerminal(terminalId)}
          title="Open this shell in its own window"
          data-testid="terminal-popout"
        >
          Pop out
        </button>
      </div>
      <TerminalPane terminalId={terminalId} active={active} onStatus={setStatus} />
    </div>
  )
}
