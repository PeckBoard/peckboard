import { PHASE_LABEL, type TerminalPhase } from '../../store/terminals'
import './Terminal.css'

// Kept apart from TerminalPane so eagerly-loaded screens (Views) can show a
// terminal's status without pulling xterm into the main bundle.

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
