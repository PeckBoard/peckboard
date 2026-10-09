import { useEffect, useState } from 'react'
import TerminalPane from './TerminalPane'
import { authedFetch, getToken } from '../../store/auth'
import './Terminal.css'

/**
 * `/terminal/:id` — the pop-out window: the same live shell, full viewport,
 * no app chrome. Shares the opener's login (same-origin token storage).
 */
export default function TerminalPopout({ terminalId }: { terminalId: string }) {
  const [name, setName] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    authedFetch(`/api/terminals/${encodeURIComponent(terminalId)}`)
      .then((r) => (r.ok ? r.json() : null))
      .then((t: { name?: string } | null) => {
        if (!cancelled && t?.name) setName(t.name)
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [terminalId])

  useEffect(() => {
    document.title = name ? `${name} — Terminal` : 'Terminal'
  }, [name])

  if (!getToken()) {
    return (
      <div className="terminal-popout" data-testid="terminal-popout-window">
        <div className="terminal-pane-overlay">
          Sign in to Peckboard in the main window, then reopen this terminal.
        </div>
      </div>
    )
  }

  return (
    <div className="terminal-popout" data-testid="terminal-popout-window">
      <TerminalPane terminalId={terminalId} active />
    </div>
  )
}
