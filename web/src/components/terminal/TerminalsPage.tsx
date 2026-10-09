import { useEffect, useState } from 'react'
import ConfirmDialog from '../ConfirmDialog'
import List from '../List'
import ListViewHeader from '../ListViewHeader'
import RenameModal from '../RenameModal'
import type { MenuItem } from '../Dropdown'
import { NotPersistentBadge, TerminalStatusPill } from './TerminalView'
import { popOutTerminal, useTerminalsStore, type TerminalInfo } from '../../store/terminals'
import { formatRelativeTime } from '../../lib/review'
import { describeActionError } from '../../utils/actionError'
import './Terminal.css'

interface Props {
  activeId: string | null
  onOpen: (id: string) => void
  onNew: () => void
  /** A terminal was closed for good — drop its tab. */
  onClosed: (id: string) => void
}

/** Every open terminal: name, host, live state, last activity. */
export default function TerminalsPage({ activeId, onOpen, onNew, onClosed }: Props) {
  const terminals = useTerminalsStore((s) => s.terminals)
  const loaded = useTerminalsStore((s) => s.loaded)
  const fetchTerminals = useTerminalsStore((s) => s.fetchTerminals)
  const rename = useTerminalsStore((s) => s.rename)
  const close = useTerminalsStore((s) => s.close)
  const [renaming, setRenaming] = useState<TerminalInfo | null>(null)
  const [confirmClose, setConfirmClose] = useState<TerminalInfo | null>(null)
  const [error, setError] = useState<string | null>(null)

  // Statuses change underneath us (reconnects, shells ending): keep the
  // list fresh while it's on screen.
  useEffect(() => {
    void fetchTerminals()
    const t = window.setInterval(() => void fetchTerminals(), 5000)
    return () => window.clearInterval(t)
  }, [fetchTerminals])

  const doClose = async () => {
    const t = confirmClose
    setConfirmClose(null)
    if (!t) return
    try {
      await close(t.id)
      onClosed(t.id)
    } catch (e) {
      setError(describeActionError(e, "Couldn't close the terminal."))
    }
  }

  const menu = (t: TerminalInfo): MenuItem[] => [
    { label: 'Open', onSelect: () => onOpen(t.id) },
    { label: 'Pop out', onSelect: () => popOutTerminal(t.id) },
    { label: 'Rename', onSelect: () => setRenaming(t) },
    { divider: true },
    { label: 'Close terminal', danger: true, onSelect: () => setConfirmClose(t) },
  ]
  return (
    <div className="list-view" data-testid="terminals-page">
      <ListViewHeader
        title="Terminals"
        actionLabel="+ New terminal"
        actionTestId="terminal-new"
        onAction={onNew}
      />
      {error && <p className="form-error">{error}</p>}
      {!loaded ? (
        <div className="list-view-body">
          <div className="list-view-empty">Loading…</div>
        </div>
      ) : (
        <List<TerminalInfo>
          items={terminals}
          getKey={(t) => t.id}
          activeId={activeId}
          onActivate={(t) => onOpen(t.id)}
          getMenuItems={menu}
          renderItem={(t) => (
            <>
              <span className="list-view-name">{t.name}</span>
              <span className="list-view-meta">
                <span className="list-view-tag">{t.host_label}</span>
                <TerminalStatusPill phase={t.status.phase} />
                {t.status.persistent === false && <NotPersistentBadge />}
                <span className="list-view-time">{formatRelativeTime(t.last_active_at)}</span>
              </span>
            </>
          )}
          emptyState={
            <div className="list-view-empty">
              <p>No terminals open</p>
              <button className="list-view-empty-action" onClick={onNew}>
                Open a terminal
              </button>
            </div>
          }
        />
      )}
      {renaming && (
        <RenameModal
          title="Rename terminal"
          label="Terminal name"
          initialValue={renaming.name}
          onSubmit={(name) => rename(renaming.id, name)}
          onClose={() => setRenaming(null)}
        />
      )}
      {confirmClose && (
        <ConfirmDialog
          title="Close terminal"
          message={`Close "${confirmClose.name}"? The remote shell is ended and anything running in it stops.`}
          confirmLabel="Close"
          cancelLabel="Cancel"
          danger
          onConfirm={doClose}
          onCancel={() => setConfirmClose(null)}
        />
      )}
    </div>
  )
}
