import { MenuButton } from '../Dropdown'

export interface PickerOption {
  value: string
  label: string
  /** Muted secondary line (e.g. the skill's description). */
  description?: string
  /** Heading the option sits under (e.g. "Groups" / "Skills"). */
  group?: { id: string; label: string }
  /** Extra text the filter matches, never shown. */
  searchText?: string
}

interface Props {
  options: PickerOption[]
  selected: string[]
  onChange: (next: string[]) => void
  /** Testid of the trigger; the filter input gets `<testId>-search`, each
   *  option `<testId>-option-<value>`, each chip `<testId>-chip-<value>`. */
  testId: string
  placeholder?: string
  emptyLabel?: string
  ariaLabel?: string
  id?: string
}

/**
 * Multi-select over database-backed choices (skills, groups): the shared
 * searchable `MenuButton` listbox toggles an option per pick, and the current
 * selection shows as removable chips below — the same shape the plugin
 * registry's tag filter uses.
 */
export default function SkillMultiPicker({
  options,
  selected,
  onChange,
  testId,
  placeholder = 'Search…',
  emptyLabel = 'Nothing to choose from yet',
  ariaLabel = 'Choose',
  id,
}: Props) {
  const sel = new Set(selected)
  const toggle = (v: string) =>
    onChange(sel.has(v) ? selected.filter((x) => x !== v) : [...selected, v])
  const byValue = new Map(options.map((o) => [o.value, o]))
  return (
    <div className="skill-picker">
      <MenuButton
        id={id}
        ariaLabel={ariaLabel}
        triggerClassName="form-input skill-picker-trigger"
        testId={testId}
        haspopup="listbox"
        align="left"
        matchTriggerWidth
        searchable
        searchPlaceholder={placeholder}
        searchTestId={`${testId}-search`}
        emptyLabel={emptyLabel}
        items={options.map((o) => ({
          label: o.label,
          description: o.description,
          group: o.group,
          searchText: o.searchText,
          active: sel.has(o.value),
          hint: sel.has(o.value) ? '✓' : undefined,
          onSelect: () => toggle(o.value),
          testId: `${testId}-option-${o.value}`,
        }))}
      >
        <span className="skill-picker-label">
          {selected.length === 0 ? 'None selected' : `${selected.length} selected`}
        </span>
        <span aria-hidden="true">▾</span>
      </MenuButton>
      {selected.length > 0 && (
        <div className="registry-active-tags skill-picker-chips">
          {selected.map((v) => (
            <button
              key={v}
              type="button"
              className="mcp-chip mcp-chip--toggle mcp-chip--on"
              title={`Remove ${byValue.get(v)?.label ?? v}`}
              data-testid={`${testId}-chip-${v}`}
              onClick={() => toggle(v)}
            >
              {byValue.get(v)?.label ?? v} ✕
            </button>
          ))}
        </div>
      )}
    </div>
  )
}
