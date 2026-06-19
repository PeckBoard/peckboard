import { useEffect, useRef, useState } from 'react'
import type { PriorityInfo } from './utils'

interface PriorityChevronProps {
  value: number
  /** Disabled cards (done / won't do) still render the icon for context but
   *  the click affordance is suppressed — matches the existing rule about
   *  not letting users repriorise terminal-step cards. */
  disabled?: boolean
  priorities: PriorityInfo[]
  onChange: (next: number) => void
}

/**
 * Priority indicator + quick-change picker for a kanban card.
 *
 * Rendered as a small icon button: a double chevron up for Critical, a
 * single up for High, a dash for Medium, and a single down for Low — all
 * tinted to the same semantic colour the old text badge used. Clicking
 * opens a tiny popover with the four options so the user can repriorise
 * without leaving the board.
 */
export default function PriorityChevron({
  value,
  disabled = false,
  priorities,
  onChange,
}: PriorityChevronProps) {
  const [open, setOpen] = useState(false)
  const wrapRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (!open) return
    const onDocMouseDown = (e: MouseEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setOpen(false)
    }
    document.addEventListener('mousedown', onDocMouseDown)
    return () => document.removeEventListener('mousedown', onDocMouseDown)
  }, [open])

  const tier = tierOf(value)

  return (
    <div className="priority-chevron-wrap" ref={wrapRef}>
      <button
        type="button"
        className={`priority-chevron priority-chevron-${tier}${disabled ? ' is-disabled' : ''}`}
        title={labelOf(value, priorities)}
        aria-label={`Priority ${labelOf(value, priorities)}`}
        disabled={disabled}
        onClick={(e) => {
          // The card body listens for clicks to toggle expand/collapse; the
          // chevron is its own affordance, so we stop the click from
          // bubbling up.
          e.stopPropagation()
          if (!disabled) setOpen((v) => !v)
        }}
      >
        <ChevronGlyph tier={tier} />
      </button>
      {open && (
        <div className="priority-chevron-menu" role="menu">
          {priorities.map((p) => {
            const pt = tierOf(p.value)
            return (
              <button
                key={p.value}
                type="button"
                className={`priority-chevron-option priority-chevron-${pt}${
                  p.value === value ? ' is-active' : ''
                }`}
                onClick={(e) => {
                  e.stopPropagation()
                  setOpen(false)
                  if (p.value !== value) onChange(p.value)
                }}
              >
                <ChevronGlyph tier={pt} />
                <span className="priority-chevron-option-label">{p.label}</span>
              </button>
            )
          })}
        </div>
      )}
    </div>
  )
}

type Tier = 'critical' | 'high' | 'medium' | 'low'

function tierOf(value: number): Tier {
  if (value <= 0) return 'critical'
  if (value <= 1) return 'high'
  if (value <= 2) return 'medium'
  return 'low'
}

function labelOf(value: number, priorities: PriorityInfo[]): string {
  return priorities.find((p) => p.value === value)?.label ?? `P${value}`
}

/** SVG glyph for the tier. Stroke uses currentColor so the surrounding
 *  className colour drives the tint. */
function ChevronGlyph({ tier }: { tier: Tier }) {
  switch (tier) {
    case 'critical':
      // Double up chevron.
      return (
        <svg
          width="12"
          height="12"
          viewBox="0 0 16 16"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
          aria-hidden="true"
        >
          <polyline points="3 9 8 4 13 9" />
          <polyline points="3 13 8 8 13 13" />
        </svg>
      )
    case 'high':
      // Single up chevron.
      return (
        <svg
          width="12"
          height="12"
          viewBox="0 0 16 16"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
          aria-hidden="true"
        >
          <polyline points="3 11 8 6 13 11" />
        </svg>
      )
    case 'medium':
      // Horizontal dash.
      return (
        <svg
          width="12"
          height="12"
          viewBox="0 0 16 16"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          aria-hidden="true"
        >
          <line x1="3" y1="8" x2="13" y2="8" />
        </svg>
      )
    case 'low':
      // Single down chevron.
      return (
        <svg
          width="12"
          height="12"
          viewBox="0 0 16 16"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
          aria-hidden="true"
        >
          <polyline points="3 6 8 11 13 6" />
        </svg>
      )
  }
}
