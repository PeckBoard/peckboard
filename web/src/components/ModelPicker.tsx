import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import type { ModelInfo } from '../store/resources'

interface ModelPickerProps {
  /** Currently-selected model id. `''` means "no override" (the default option). */
  value: string
  /** Called with the chosen model id (`''` for the default option). */
  onChange: (id: string) => void
  /** Flat model catalogue, e.g. `useResourcesStore(s => s.models)`. */
  models: ModelInfo[]
  /** Label for the empty/`''` option that clears the override. */
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
  /** Shown disabled at the top of an empty list (e.g. "Loading models…"). */
  emptyHint?: string
  /** Fired when the popup opens — e.g. to (re)fetch the catalogue. */
  onOpen?: () => void
}

const POPUP_MARGIN = 8
const POPUP_MAX_HEIGHT = 320

/**
 * Searchable model combobox. A trigger button that opens a portal popup with a
 * filter input over the model catalogue — type any part of a model's name or
 * id to narrow the list. Replaces the plain `<select>` / `MenuButton` model
 * dropdowns so every model-selection surface (new session, session toolbar,
 * project pages, automation) filters the same way. With providers like Cursor
 * exposing 100+ models, an unfiltered list is unusable.
 */
export default function ModelPicker({
  value,
  onChange,
  models,
  defaultLabel = 'Auto',
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
  const [open, setOpen] = useState(false)
  const [query, setQuery] = useState('')
  const [highlight, setHighlight] = useState(0)
  const [pos, setPos] = useState<{ left: number; top: number; width: number } | null>(null)

  const triggerRef = useRef<HTMLButtonElement | null>(null)
  const popupRef = useRef<HTMLDivElement | null>(null)
  const inputRef = useRef<HTMLInputElement | null>(null)

  const selectedLabel =
    valueLabel ??
    (value ? (models.find((m) => m.id === value)?.display_name ?? value) : defaultLabel)

  // The default ("") option behaves like any other row, so arrow-key
  // navigation and Enter work uniformly. It's filtered out once the user
  // types something that doesn't match its label.
  const options = useMemo<ModelInfo[]>(() => {
    const q = query.trim().toLowerCase()
    const withDefault: ModelInfo[] = [{ id: '', display_name: defaultLabel }, ...models]
    if (!q) return withDefault
    return withDefault.filter(
      (m) => m.display_name.toLowerCase().includes(q) || m.id.toLowerCase().includes(q),
    )
  }, [models, query, defaultLabel])

  const place = () => {
    const el = triggerRef.current
    if (!el) return
    const r = el.getBoundingClientRect()
    const width = Math.max(r.width, 240)
    const vw = window.innerWidth
    const vh = window.innerHeight
    let left = align === 'right' ? r.right - width : r.left
    if (left + width > vw - POPUP_MARGIN) left = vw - width - POPUP_MARGIN
    if (left < POPUP_MARGIN) left = POPUP_MARGIN
    // Prefer opening downward; flip above the trigger if it would overflow.
    let top = r.bottom + 4
    if (top + POPUP_MAX_HEIGHT > vh - POPUP_MARGIN) {
      const above = r.top - 4 - POPUP_MAX_HEIGHT
      if (above > POPUP_MARGIN) top = r.top - 4 - POPUP_MAX_HEIGHT
    }
    setPos({ left, top, width })
  }

  // Position before paint to avoid a flash at the wrong spot.
  useLayoutEffect(() => {
    if (open) place()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  useEffect(() => {
    if (!open) return
    inputRef.current?.focus()
    const onDown = (e: MouseEvent) => {
      const t = e.target as HTMLElement | null
      if (t?.closest('.model-picker-popup') || t === triggerRef.current) return
      setOpen(false)
    }
    const reflow = () => place()
    document.addEventListener('mousedown', onDown)
    window.addEventListener('resize', reflow)
    window.addEventListener('scroll', reflow, true)
    return () => {
      document.removeEventListener('mousedown', onDown)
      window.removeEventListener('resize', reflow)
      window.removeEventListener('scroll', reflow, true)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  const toggle = () => {
    if (disabled) return
    setQuery('')
    setOpen((o) => {
      const next = !o
      if (next) {
        // Start the keyboard cursor on the current selection (index 0 is the
        // default option, real models follow). Reset every time we open since
        // the query is cleared back to the full list.
        const idx = value ? models.findIndex((m) => m.id === value) : -1
        setHighlight(idx >= 0 ? idx + 1 : 0)
        onOpen?.()
      }
      return next
    })
  }

  const choose = (modelId: string) => {
    onChange(modelId)
    setOpen(false)
  }

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Escape') {
      e.preventDefault()
      setOpen(false)
      triggerRef.current?.focus()
    } else if (e.key === 'ArrowDown') {
      e.preventDefault()
      setHighlight((h) => Math.min(h + 1, options.length - 1))
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      setHighlight((h) => Math.max(h - 1, 0))
    } else if (e.key === 'Enter') {
      e.preventDefault()
      const opt = options[highlight]
      if (opt) choose(opt.id)
    }
  }

  return (
    <>
      <button
        ref={triggerRef}
        type="button"
        id={id}
        className={triggerClassName}
        aria-label={ariaLabel}
        aria-haspopup="listbox"
        aria-expanded={open}
        disabled={disabled}
        data-testid={testId}
        onClick={toggle}
      >
        <span className="model-picker-value">{selectedLabel}</span>
        {showChevron && (
          <span className="model-picker-chev" aria-hidden="true">
            ▾
          </span>
        )}
      </button>
      {open &&
        pos &&
        createPortal(
          <div
            ref={popupRef}
            className="model-picker-popup dropdown-menu"
            style={{ position: 'fixed', left: pos.left, top: pos.top, width: pos.width }}
          >
            <input
              ref={inputRef}
              className="model-picker-search"
              type="text"
              autoFocus
              value={query}
              placeholder="Search models…"
              aria-label="Search models"
              onChange={(e) => {
                setQuery(e.target.value)
                setHighlight(0)
              }}
              onKeyDown={onKeyDown}
              data-testid={testId ? `${testId}-search` : undefined}
            />
            <div className="model-picker-list" role="listbox">
              {options.length === 0 ? (
                <button type="button" className="dropdown-item" disabled>
                  {emptyHint && models.length === 0 ? emptyHint : 'No matches'}
                </button>
              ) : (
                options.map((m, idx) => (
                  <button
                    key={m.id || '__default__'}
                    type="button"
                    role="option"
                    aria-selected={m.id === value}
                    className={`dropdown-item${m.id === value ? ' dropdown-item-active' : ''}${
                      idx === highlight ? ' model-picker-item-highlight' : ''
                    }`}
                    onMouseEnter={() => setHighlight(idx)}
                    onClick={() => choose(m.id)}
                    data-testid={testId ? `${testId}-option-${m.id || 'default'}` : undefined}
                  >
                    <span className="dropdown-item-label">{m.display_name}</span>
                  </button>
                ))
              )}
            </div>
          </div>,
          document.body,
        )}
    </>
  )
}
