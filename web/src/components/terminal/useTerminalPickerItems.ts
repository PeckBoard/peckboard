import { useState } from 'react'
import type { MenuItem } from '../Dropdown'
import { useTerminalsStore, type TerminalHost, type TerminalInfo } from '../../store/terminals'
import { describeActionError } from '../../utils/actionError'

/**
 * Rows for a terminal picker: the user's open terminals, then "New terminal
 * on <host>" rows (which open a fresh shell first). Call `refresh` when the
 * popup opens. Shared by `TerminalPicker` and the Views "Add widget" menu.
 */
export function useTerminalPickerItems(testId: string, onPick: (t: TerminalInfo) => void) {
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
  return { items, refresh, busy, error }
}
