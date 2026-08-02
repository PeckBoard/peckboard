import { useMemo } from 'react'
import { MenuButton, type MenuItem } from './Dropdown'
import { useResourcesStore, type ModelInfo } from '../store/resources'

interface ModelPickerProps {
  /** Currently-selected model id. `''` means "no override" (the default option). */
  value: string
  /** Called with the chosen model id (`''` for the default option). */
  onChange: (id: string) => void
  /** Flat model catalogue, e.g. `useResourcesStore(s => s.models)`. */
  models: ModelInfo[]
  /** Label for the empty/`''` option. When omitted, no empty row is
   *  rendered — the picker only offers real models. */
  defaultLabel?: string
  /** Override the trigger's text (e.g. a prefix-stripped name). Falls back to
   *  the selected model's display name, then the raw id, then `defaultLabel`. */
  valueLabel?: string
  /** Class for the trigger button. Defaults to form-field (`<select>`) styling. */
  triggerClassName?: string
  /** Show the trailing ▾ chevron on the trigger. */
  showChevron?: boolean
  /** Horizontal alignment of the popup against the trigger. */
  align?: 'left' | 'right'
  ariaLabel?: string
  id?: string
  disabled?: boolean
  testId?: string
  /** Shown disabled at the top of an empty catalogue (e.g. "Loading models…"). */
  emptyHint?: string
  /** Fired when the popup opens — e.g. to (re)fetch the catalogue. */
  onOpen?: () => void
}

/** The popup never gets narrower than this, however small the trigger is. */
const POPUP_MIN_WIDTH = 240

/**
 * Searchable model combobox. A trigger button that opens a portal popup with a
 * filter input over the model catalogue — type any part of a model's name or
 * id to narrow the list. Replaces the plain `<select>` / `MenuButton` model
 * dropdowns so every model-selection surface (new session, session toolbar,
 * project pages, automation) filters the same way. With providers like Cursor
 * exposing 100+ models, an unfiltered list is unusable.
 *
 * The popup itself is the shared `MenuButton`/`Dropdown` primitive in its
 * `searchable` (combobox + listbox) mode — this file only maps the catalogue
 * to `MenuItem`s and renders the trigger's label. There is no second popup
 * implementation to keep in sync.
 */
export default function ModelPicker({
  value,
  onChange,
  models,
  defaultLabel,
  valueLabel,
  triggerClassName = 'form-input model-picker-trigger',
  showChevron = true,
  align = 'left',
  ariaLabel = 'Select model',
  id,
  disabled,
  testId,
  emptyHint,
  onOpen,
}: ModelPickerProps) {
  const selectedLabel =
    valueLabel ??
    (value
      ? (models.find((m) => m.id === value)?.display_name ?? value)
      : (defaultLabel ?? 'Select model…'))

  // When a `defaultLabel` is given, the empty ("") option is just another
  // row, so arrow-key navigation and Enter work uniformly over it. Without
  // one there is no empty row — the picker only offers real models.
  const providers = useResourcesStore((s) => s.providers)

  // When a `defaultLabel` is given, the empty ("") option is just another
  // row, so arrow-key navigation and Enter work uniformly over it. Without
  // one there is no empty row — the picker only offers real models.
  const items = useMemo<MenuItem[]>(() => {
    const defaultRow = defaultLabel === undefined ? [] : [{ id: '', display_name: defaultLabel }]
    const rows: MenuItem[] = [...defaultRow, ...models].map((m) => {
      // Model ids are `provider:model`; group rows under their provider
      // section. Ids without a known provider prefix (a reuse like
      // SystemPromptPicker, or the empty default row) stay ungrouped.
      const providerId = m.id.includes(':') ? m.id.split(':')[0] : undefined
      const provider = providerId ? providers.find((p) => p.id === providerId) : undefined
      const unconfigured = provider?.configured === false
      return {
        label: m.display_name,
        // The raw id carries the provider and account, so typing "cursor" or an
        // account name narrows the list even though neither is in the label.
        searchText: m.id,
        active: m.id === value,
        onSelect: () => onChange(m.id),
        testId: testId ? `${testId}-option-${m.id || 'default'}` : undefined,
        group: provider
          ? {
              id: provider.id,
              label: provider.display_name,
              tag: unconfigured ? 'not configured' : undefined,
            }
          : undefined,
        dimmed: unconfigured,
      }
    })
    // Nothing but the default option means the catalogue never arrived; say
    // so rather than leaving the user staring at a one-row list.
    if (models.length === 0 && emptyHint) rows.push({ label: emptyHint, disabled: true })
    return rows
  }, [models, providers, defaultLabel, value, onChange, testId, emptyHint])

  // Warn-only hint when a listed provider has no detected auth: host-level
  // logins can exist that the backend can't see, so selection is never
  // blocked — the user just gets pointed at Settings before a worker
  // spawns and dies on auth.
  const hasUnconfigured = useMemo(
    () =>
      providers.some(
        (p) => p.configured === false && models.some((m) => m.id.startsWith(`${p.id}:`)),
      ),
    [providers, models],
  )

  return (
    <MenuButton
      footer={
        hasUnconfigured ? (
          <span className="model-picker-config-hint">
            Some providers aren&apos;t configured.{' '}
            <a href="/settings">Settings → Providers &amp; Accounts</a>
          </span>
        ) : undefined
      }
      items={items}
      ariaLabel={ariaLabel}
      triggerClassName={triggerClassName}
      align={align}
      id={id}
      disabled={disabled}
      testId={testId}
      onOpen={onOpen}
      searchable
      searchPlaceholder="Search models…"
      searchTestId={testId ? `${testId}-search` : undefined}
      listLabel={ariaLabel}
      emptyLabel={emptyHint}
      haspopup="listbox"
      matchTriggerWidth
      minWidth={POPUP_MIN_WIDTH}
    >
      <span className="model-picker-value">{selectedLabel}</span>
      {showChevron && (
        <span className="model-picker-chev" aria-hidden="true">
          ▾
        </span>
      )}
    </MenuButton>
  )
}
