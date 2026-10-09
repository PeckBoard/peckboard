import { useState } from 'react'
import { MenuButton, type MenuItem } from '../Dropdown'
import { useTerminalsStore, type TerminalHost, type TerminalInfo } from '../../store/terminals'
import { describeActionError } from '../../utils/actionError'

/**
 * Searchable single-choice terminal picker: the user's open terminals, then
 * "New terminal on <host>" rows (which open a fresh shell first). Picking
 * the same host twice gives two separate shells; picking an already-shown
 * terminal mirrors that one live shell.
 */
export default function TerminalPicker({
  onPick,
  label,
  testId,
  disabled,
  className,
}: {
  onPick: (t: TerminalInfo) => void
  label: string
  testId: string
  disabled?: boolean
  className?: string
}) {
  const terminals = useTerminalsStore((s) => s.terminals)
  const fetchTerminals = useTerminalsStore((s) => s.fetchTerminals)
  const fetchHosts = useTerminalsStore((s) => s.fetchHosts)
  const create = useTerminalsStore((s) => s.create)
  const [hosts, setHosts] = useState<TerminalHost[]>([])
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')

  const refresh = () => {
    void fetchTerminals()
    // Opening shells is admin-only; for everyone else this 403s and the
    // picker just lists their open terminals.
    fetchHosts()
      .then(setHosts)
      .catch(() => setHosts([]))
  }

  const openOn = async (h: TerminalHost) => {
    setBusy(true)
    setError('')
    try {
      onPick(await create(h.plugin_id, h.id))
    } catch (e) {
      setError(describeActionError(e, "Couldn't open a terminal."))
    } finally {
      setBusy(false)
    }
  }

  const items: MenuItem[] = [
    ...terminals.map((t) => ({
      label: t.name,
      hint: t.host_label,
      searchText: `${t.id} ${t.host_label}`,
      group: { id: 'open', label: 'Open terminals' },
      testId: `${testId}-terminal-${t.id}`,
      onSelect: () => onPick(t),
    })),
    ...hosts.map((h) => ({
      label: `New terminal on ${h.label}`,
      hint: `${h.username}@${h.hostname}${h.port === 22 ? '' : `:${h.port}`}`,
      searchText: [h.hostname, h.username, ...h.tags].join(' '),
      group: { id: 'new', label: 'New terminal on host…' },
      testId: `${testId}-host-${h.id}`,
      onSelect: () => void openOn(h),
    })),
  ]
  return (
    <>
      <MenuButton
        items={items}
        searchable
        searchPlaceholder="Search terminals and hosts…"
        searchTestId={`${testId}-search`}
        emptyLabel="No terminals or hosts"
        listLabel="Terminals"
        haspopup="listbox"
        ariaLabel={label}
        triggerClassName={className ?? 'btn-secondary btn-sm'}
        testId={testId}
        disabled={disabled || busy}
        minWidth={300}
        onOpen={refresh}
      >
        {busy ? 'Opening…' : label}
      </MenuButton>
      {error && (
        <span className="form-error" role="alert" data-testid={`${testId}-error`}>
          {error}
        </span>
      )}
    </>
  )
}
