import { useMemo, useRef, useState } from 'react'
import Dropdown, { type MenuItem } from './Dropdown'

interface Props {
  /** IANA zone name, or '' for UTC. */
  value: string
  onChange: (tz: string) => void
  disabled?: boolean
  id?: string
}

const UTC_LABEL = 'UTC'

/** Every IANA zone the runtime knows about, UTC first. Falls back to a
 *  short hand-picked list on runtimes without `Intl.supportedValuesOf`
 *  (older WebKit). */
function allTimezones(): string[] {
  const supportedValuesOf = (Intl as unknown as { supportedValuesOf?: (key: string) => string[] })
    .supportedValuesOf
  if (typeof supportedValuesOf === 'function') {
    return supportedValuesOf('timeZone')
  }
  return [
    'America/New_York',
    'America/Chicago',
    'America/Denver',
    'America/Los_Angeles',
    'America/Sao_Paulo',
    'Europe/London',
    'Europe/Paris',
    'Europe/Berlin',
    'Europe/Moscow',
    'Africa/Cairo',
    'Asia/Dubai',
    'Asia/Kolkata',
    'Asia/Shanghai',
    'Asia/Tokyo',
    'Australia/Sydney',
    'Pacific/Auckland',
  ]
}

/**
 * Searchable IANA timezone picker, built on the same `Dropdown searchable`
 * primitive as `ModelPicker` / `WorkflowSelect` — a field-shaped trigger
 * button that opens a portal popup with a filter input. `''` means UTC,
 * matching the backend's `timezone: null` = UTC convention.
 */
export default function TimezoneSelect({ value, onChange, disabled, id }: Props) {
  const [anchor, setAnchor] = useState<{ x: number; y: number; width: number } | null>(null)
  const triggerRef = useRef<HTMLButtonElement>(null)

  const zones = useMemo(() => allTimezones(), [])
  const triggerLabel = value || UTC_LABEL

  const open = () => {
    const el = triggerRef.current
    if (!el || disabled) return
    const r = el.getBoundingClientRect()
    setAnchor({ x: r.left, y: r.bottom + 4, width: r.width })
  }
  const close = () => setAnchor(null)

  const items: MenuItem[] = [
    { label: UTC_LABEL, active: value === '', onSelect: () => onChange('') },
    ...zones.map(
      (tz) =>
        ({ label: tz, active: value === tz, onSelect: () => onChange(tz) }) satisfies MenuItem,
    ),
  ]

  return (
    <div className="timezone-select">
      <button
        ref={triggerRef}
        type="button"
        id={id}
        className="form-input"
        onClick={() => (anchor ? close() : open())}
        disabled={disabled}
        data-testid="repeating-task-timezone"
      >
        {triggerLabel}
      </button>
      {anchor && (
        <Dropdown
          anchor={{ x: anchor.x, y: anchor.y }}
          items={items}
          onClose={close}
          align="left"
          searchable
          searchPlaceholder="Search timezones…"
          minWidth={anchor.width}
          maxWidth={anchor.width}
        />
      )}
    </div>
  )
}
