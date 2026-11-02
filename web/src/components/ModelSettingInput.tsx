import { useEffect, useId, useMemo, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { useResourcesStore, type ModelInfo } from '../store/resources'

interface Props {
  value: string
  placeholder?: string
  testId?: string
  onChange: (value: string) => void
  /** Persist now — a catalog entry was picked. */
  onCommit: (value: string) => void
  /** Persist on blur / Enter, only when the value changed (typed ids). */
  onCommitIfDirty: (value: string) => void
}

/** Most options rendered at once; narrowing the search shows the rest. */
const MAX_OPTIONS = 100

function matches(model: ModelInfo, tokens: string[]): boolean {
  const hay = `${model.id} ${model.display_name}`.toLowerCase()
  return tokens.every((t) => hay.includes(t))
}

/**
 * Model-id input for `type: "model"` plugin settings: a free-text field with
 * a searchable listbox over the live `/api/models` catalog (the same one the
 * session model picker reads, account-scoped `@account` ids included).
 * Picking an entry saves it; a typed id that isn't in the catalog (an
 * offline or undiscovered model) still saves on blur / Enter, and a stored
 * value missing from the catalog is kept and flagged rather than blanked.
 * Combobox ARIA and popup skin follow `PathAutocomplete`.
 */
export default function ModelSettingInput({
  value,
  placeholder,
  testId,
  onChange,
  onCommit,
  onCommitIfDirty,
}: Props) {
  const models = useResourcesStore((s) => s.models)
  const modelsFailed = useResourcesStore((s) => s.resourceErrors.models)
  const fetchModels = useResourcesStore((s) => s.fetchModels)
  const inputRef = useRef<HTMLInputElement | null>(null)
  const popupRef = useRef<HTMLDivElement | null>(null)
  // The id a pick already saved, so the blur that follows doesn't save it twice.
  const pickedRef = useRef<string | null>(null)
  const [open, setOpen] = useState(false)
  // Search text; `null` = not searching (the field shows the stored value).
  const [query, setQuery] = useState<string | null>(null)
  const [highlight, setHighlight] = useState(0)
  const [rect, setRect] = useState<{ left: number; top: number; width: number } | null>(null)
  const baseId = useId().replace(/:/g, '')
  const listId = `${baseId}-list`

  useEffect(() => {
    if (models.length === 0) void fetchModels()
    // Only on mount: the store is shared, a refetch per render would storm.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const filtered = useMemo(() => {
    const tokens = (query ?? '').toLowerCase().split(/\s+/).filter(Boolean)
    return models.filter((m) => matches(m, tokens)).slice(0, MAX_OPTIONS)
  }, [models, query])

  const current = value.trim()
  const known = current ? models.find((m) => m.id === current) : undefined
  const notInCatalog = !!current && models.length > 0 && !known

  useEffect(() => {
    if (!open) return
    const place = () => {
      const r = inputRef.current?.getBoundingClientRect()
      if (r) setRect({ left: r.left, top: r.bottom + 4, width: r.width })
    }
    place()
    window.addEventListener('resize', place)
    window.addEventListener('scroll', place, true)
    return () => {
      window.removeEventListener('resize', place)
      window.removeEventListener('scroll', place, true)
    }
  }, [open])

  useEffect(() => {
    if (!open) return
    popupRef.current
      ?.querySelector('.model-picker-item-highlight')
      ?.scrollIntoView({ block: 'nearest' })
  }, [open, highlight])

  const pick = (m: ModelInfo) => {
    pickedRef.current = m.id
    onChange(m.id)
    onCommit(m.id)
    setOpen(false)
    setQuery(null)
  }

  const onKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (open && e.key === 'ArrowDown') {
      e.preventDefault()
      setHighlight((h) => Math.min(h + 1, Math.max(filtered.length - 1, 0)))
    } else if (open && e.key === 'ArrowUp') {
      e.preventDefault()
      setHighlight((h) => Math.max(h - 1, 0))
    } else if (e.key === 'Enter') {
      e.preventDefault()
      // A typed search that matches something picks the highlighted entry;
      // otherwise Enter keeps the text as typed (free-text fallback).
      if (open && query && filtered[highlight]) pick(filtered[highlight])
      else e.currentTarget.blur()
    } else if (e.key === 'Escape' && open) {
      // With the popup open, Escape closes it, not the hosting Modal.
      e.preventDefault()
      e.stopPropagation()
      setOpen(false)
    } else if (!open && e.key === 'ArrowDown') {
      setOpen(true)
    }
  }

  const activeOptionId = open && filtered.length > 0 ? `${baseId}-opt-${highlight}` : undefined

  return (
    <>
      <input
        ref={inputRef}
        className="plugin-setting-input"
        type="text"
        role="combobox"
        aria-expanded={open}
        aria-controls={listId}
        aria-activedescendant={activeOptionId}
        aria-autocomplete="list"
        autoComplete="off"
        spellCheck={false}
        placeholder={placeholder || 'Search models or type provider:model'}
        value={value}
        data-testid={testId}
        onChange={(e) => {
          pickedRef.current = null
          onChange(e.target.value)
          setQuery(e.target.value)
          setHighlight(0)
          setOpen(true)
        }}
        onFocus={() => {
          // Focus opens the full catalog, not one filtered to the stored id.
          setQuery('')
          setHighlight(0)
          setOpen(true)
        }}
        onBlur={(e) => {
          setOpen(false)
          setQuery(null)
          if (pickedRef.current !== e.target.value) onCommitIfDirty(e.target.value)
        }}
        onKeyDown={onKeyDown}
      />
      {known && (
        <span className="plugin-setting-desc" data-testid={testId && `${testId}-name`}>
          {known.display_name}
        </span>
      )}
      {notInCatalog && (
        <span
          className="plugin-setting-desc plugin-setting-not-in-catalog"
          data-testid={testId && `${testId}-not-in-catalog`}
        >
          Not in catalog — kept as entered.
        </span>
      )}
      {open &&
        rect &&
        createPortal(
          <div
            ref={popupRef}
            className="dropdown-menu model-picker-popup"
            style={{ position: 'fixed', left: rect.left, top: rect.top, width: rect.width }}
          >
            <div className="model-picker-list" id={listId} role="listbox" aria-label="Models">
              {filtered.map((m, i) => (
                <button
                  key={m.id}
                  id={`${baseId}-opt-${i}`}
                  type="button"
                  role="option"
                  aria-selected={i === highlight}
                  className={`dropdown-item${i === highlight ? ' model-picker-item-highlight' : ''}`}
                  data-testid={`model-option-${m.id}`}
                  // Keep focus in the input so blur doesn't close the popup
                  // before the click lands.
                  onMouseDown={(e) => e.preventDefault()}
                  onClick={() => pick(m)}
                  onMouseEnter={() => setHighlight(i)}
                >
                  <span className="dropdown-item-row">
                    <span className="dropdown-item-label">{m.display_name}</span>
                    <span className="dropdown-item-hint">{m.id}</span>
                  </span>
                </button>
              ))}
              {filtered.length === 0 && (
                <div className="dropdown-item-hint model-picker-empty" role="presentation">
                  {models.length === 0
                    ? modelsFailed
                      ? 'Model catalog unavailable — type an id.'
                      : 'Loading models…'
                    : 'No catalog match — press Enter to use as typed.'}
                </div>
              )}
            </div>
          </div>,
          document.body,
        )}
    </>
  )
}
