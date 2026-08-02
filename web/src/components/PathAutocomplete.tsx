import { useEffect, useId, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { authedFetch } from '../store/auth'

interface DirSuggestion {
  name: string
  path: string
}

interface Props {
  value: string
  onChange: (value: string) => void
  /** Enter pressed while the suggestion popup is closed — submit the form. */
  onSubmit?: () => void
  /** Latest server verdict on the typed path: does it exist as a directory?
   *  `null` while the field is blank or the path isn't absolute yet. */
  onExistsChange?: (exists: boolean | null) => void
  placeholder?: string
  testId?: string
}

const DEBOUNCE_MS = 200

/**
 * Absolute-path input with server-backed directory typeahead, for the
 * admin folder-registration flow. The input stays a plain free-text field —
 * typing and pasting always work — while a listbox of subdirectory
 * suggestions (from `GET /api/folders/browse`) assists. Combobox + listbox
 * ARIA mirrors the searchable `Dropdown` variant, and the popup reuses its
 * skin; `Dropdown` itself is trigger-button-anchored, so it can't wrap a
 * form field that must keep accepting free text.
 */
export default function PathAutocomplete({
  value,
  onChange,
  onSubmit,
  onExistsChange,
  placeholder,
  testId,
}: Props) {
  const inputRef = useRef<HTMLInputElement | null>(null)
  const popupRef = useRef<HTMLDivElement | null>(null)
  const [suggestions, setSuggestions] = useState<DirSuggestion[]>([])
  const [open, setOpen] = useState(false)
  const [highlight, setHighlight] = useState(0)
  const [rect, setRect] = useState<{ left: number; top: number; width: number } | null>(null)
  // Monotonic request counter so a slow response for an older keystroke
  // can't overwrite the suggestions for the current one.
  const seq = useRef(0)
  const baseId = useId().replace(/:/g, '')
  const listId = `${baseId}-list`
  const activeOptionId = open && suggestions.length > 0 ? `${baseId}-opt-${highlight}` : undefined

  useEffect(() => {
    const typed = value.trim()
    const mine = ++seq.current
    const timer = setTimeout(async () => {
      if (!typed.startsWith('/')) {
        setSuggestions([])
        setOpen(false)
        onExistsChange?.(null)
        return
      }
      try {
        const res = await authedFetch(`/api/folders/browse?path=${encodeURIComponent(typed)}`)
        if (mine !== seq.current) return
        if (!res.ok) {
          setSuggestions([])
          setOpen(false)
          onExistsChange?.(null)
          return
        }
        const data = (await res.json()) as { exists: boolean; dirs: DirSuggestion[] }
        if (mine !== seq.current) return
        onExistsChange?.(data.exists)
        setSuggestions(data.dirs)
        setHighlight(0)
        // Only pop the list while the user is actually in the field — a
        // response landing after blur must not reopen it.
        setOpen(data.dirs.length > 0 && document.activeElement === inputRef.current)
      } catch {
        if (mine === seq.current) {
          setSuggestions([])
          setOpen(false)
          onExistsChange?.(null)
        }
      }
    }, DEBOUNCE_MS)
    return () => clearTimeout(timer)
    // onExistsChange is a stable setter in practice; re-running on identity
    // churn would refetch on every parent render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [value])

  // Anchor the fixed portal popup under the input.
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
  }, [open, suggestions])

  const pick = (s: DirSuggestion) => {
    onChange(s.path)
    setHighlight(0)
    inputRef.current?.focus()
  }

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (open && suggestions.length > 0) {
      if (e.key === 'ArrowDown') {
        e.preventDefault()
        setHighlight((h) => Math.min(h + 1, suggestions.length - 1))
        return
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault()
        setHighlight((h) => Math.max(h - 1, 0))
        return
      }
      if (e.key === 'Enter') {
        e.preventDefault()
        pick(suggestions[highlight])
        return
      }
      if (e.key === 'Escape') {
        // Swallow it: with the popup open, Escape closes the popup, not the
        // Modal hosting the form (same contract as Dropdown).
        e.preventDefault()
        e.stopPropagation()
        setOpen(false)
        return
      }
    } else if (e.key === 'Enter') {
      onSubmit?.()
    }
  }

  useEffect(() => {
    if (!open) return
    popupRef.current
      ?.querySelector('.model-picker-item-highlight')
      ?.scrollIntoView({ block: 'nearest' })
  }, [open, highlight])

  return (
    <>
      <input
        ref={inputRef}
        className="form-input"
        type="text"
        role="combobox"
        aria-expanded={open}
        aria-controls={listId}
        aria-activedescendant={activeOptionId}
        aria-autocomplete="list"
        autoComplete="off"
        spellCheck={false}
        placeholder={placeholder}
        value={value}
        data-testid={testId}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={onKeyDown}
        onFocus={() => {
          if (suggestions.length > 0) setOpen(true)
        }}
        onBlur={() => setOpen(false)}
      />
      {open &&
        rect &&
        createPortal(
          <div
            ref={popupRef}
            className="dropdown-menu model-picker-popup"
            style={{ position: 'fixed', left: rect.left, top: rect.top, width: rect.width }}
          >
            <div
              className="model-picker-list"
              id={listId}
              role="listbox"
              aria-label="Directory suggestions"
            >
              {suggestions.map((s, i) => (
                <button
                  key={s.path}
                  id={`${baseId}-opt-${i}`}
                  type="button"
                  role="option"
                  aria-selected={i === highlight}
                  className={`dropdown-item${i === highlight ? ' model-picker-item-highlight' : ''}`}
                  data-testid={`path-suggestion-${s.name}`}
                  // Fill on mousedown-before-blur: default-preventing here
                  // keeps focus in the input so the blur handler doesn't
                  // close the popup before the click lands.
                  onMouseDown={(e) => e.preventDefault()}
                  onClick={() => pick(s)}
                  onMouseEnter={() => setHighlight(i)}
                >
                  <span className="dropdown-item-row">
                    <span className="dropdown-item-label">{s.name}</span>
                    <span className="dropdown-item-hint">{s.path}</span>
                  </span>
                </button>
              ))}
            </div>
          </div>,
          document.body,
        )}
    </>
  )
}
