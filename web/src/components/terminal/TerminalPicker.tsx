import { MenuButton } from '../Dropdown'
import type { TerminalInfo } from '../../store/terminals'
import { useTerminalPickerItems } from './useTerminalPickerItems'

/**
 * Searchable single-choice terminal picker. Picking the same host twice
 * gives two separate shells; picking an already-shown terminal mirrors that
 * one live shell.
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
  const { items, refresh, busy, error } = useTerminalPickerItems(testId, onPick)
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
