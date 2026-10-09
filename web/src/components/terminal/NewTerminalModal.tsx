import { useEffect, useRef, useState } from 'react'
import Modal from '../Modal'
import Dropdown, { type MenuItem } from '../Dropdown'
import { useTerminalsStore, type TerminalHost, type TerminalInfo } from '../../store/terminals'
import { describeActionError } from '../../utils/actionError'
import './Terminal.css'

interface Props {
  onClose: () => void
  /** The terminal was created — open its tab. */
  onCreated: (t: TerminalInfo) => void
}

/**
 * "New terminal": pick a host (searchable — fleets get long) and the shell
 * opens in a tab straight away. Hosts come from plugins (SSH Fleet) and carry
 * no credentials; the picker opens on its own so the flow is
 * shortcut → type a few letters → Enter.
 */
export default function NewTerminalModal({ onClose, onCreated }: Props) {
  const fetchHosts = useTerminalsStore((s) => s.fetchHosts)
  const create = useTerminalsStore((s) => s.create)
  const [hosts, setHosts] = useState<TerminalHost[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [anchor, setAnchor] = useState<{ x: number; y: number; width: number } | null>(null)
  const triggerRef = useRef<HTMLButtonElement>(null)

  const openPicker = () => {
    const el = triggerRef.current
    if (!el) return
    const r = el.getBoundingClientRect()
    setAnchor({ x: r.left, y: r.bottom + 4, width: r.width })
  }

  useEffect(() => {
    let cancelled = false
    fetchHosts()
      .then((hs) => {
        if (cancelled) return
        setHosts(hs)
        if (hs.length > 0) requestAnimationFrame(openPicker)
      })
      .catch((e: unknown) => {
        if (!cancelled) {
          setHosts([])
          setError(describeActionError(e, "Couldn't load hosts."))
        }
      })
    return () => {
      cancelled = true
    }
  }, [fetchHosts])

  const pick = async (host: TerminalHost) => {
    setAnchor(null)
    setBusy(true)
    setError(null)
    try {
      onCreated(await create(host.plugin_id, host.id))
    } catch (e) {
      setError(describeActionError(e, "Couldn't open a terminal."))
      setBusy(false)
    }
  }

  const items: MenuItem[] = (hosts ?? []).map((h) => ({
    label: h.label,
    hint: `${h.username}@${h.hostname}${h.port === 22 ? '' : `:${h.port}`}`,
    searchText: [h.hostname, h.username, ...h.tags].join(' '),
    testId: `terminal-host-${h.id}`,
    onSelect: () => void pick(h),
  }))

  return (
    <Modal onClose={onClose} maxWidth={480} data-testid="new-terminal-modal">
      <h2>New terminal</h2>
      <div className="form-field">
        <label className="form-label">Host</label>
        <button
          ref={triggerRef}
          type="button"
          className="form-input"
          onClick={() => (anchor ? setAnchor(null) : openPicker())}
          disabled={busy || !hosts || hosts.length === 0}
          data-testid="terminal-host-picker"
        >
          {busy ? 'Opening…' : hosts === null ? 'Loading hosts…' : 'Choose a host…'}
        </button>
        {hosts !== null && hosts.length === 0 && !error && (
          <p className="terminal-host-option-meta" data-testid="terminal-no-hosts">
            No SSH hosts yet. Add one on the SSH Fleet page, then come back.
          </p>
        )}
        {error && <p className="form-error">{error}</p>}
      </div>
      <div className="form-actions">
        <button type="button" className="btn-secondary" onClick={onClose} disabled={busy}>
          Cancel
        </button>
      </div>
      {anchor && (
        <Dropdown
          anchor={{ x: anchor.x, y: anchor.y }}
          items={items}
          onClose={() => setAnchor(null)}
          align="left"
          searchable
          searchPlaceholder="Search hosts…"
          searchTestId="terminal-host-search"
          listLabel="Hosts"
          minWidth={anchor.width}
          maxWidth={Math.max(anchor.width, 420)}
        />
      )}
    </Modal>
  )
}
