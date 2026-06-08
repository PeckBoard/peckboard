import { useLayoutEffect, useRef, useState } from 'react'

export interface ContextMenuItem {
  /** Display text. */
  label: string
  /** Invoked when the user clicks the item. */
  onSelect: () => void
  /** Render in the danger style (red). */
  danger?: boolean
  /** Greyed out and non-interactive. */
  disabled?: boolean
  /** Skip rendering. Lets callers express "this action only applies to
   *  sessions" without splitting the list. */
  hidden?: boolean
}

interface Anchor {
  x: number
  y: number
}

/**
 * Portal-friendly context-menu popup positioned at viewport coords.
 * Anchored, then clamped to the viewport so a right-click near an edge
 * doesn't render off-screen. Caller (typically `useContextMenu`) owns
 * the dismissal logic — this component is purely presentational.
 */
export default function ContextMenuView({
  anchor,
  items,
  onClose,
}: {
  anchor: Anchor
  items: ContextMenuItem[]
  onClose: () => void
}) {
  const ref = useRef<HTMLDivElement | null>(null)
  const [pos, setPos] = useState<Anchor>(anchor)
  const visibleItems = items.filter((i) => !i.hidden)

  // Clamp to the viewport. Layout effect so the user never sees the
  // unclamped frame paint.
  useLayoutEffect(() => {
    const el = ref.current
    if (!el) return
    const rect = el.getBoundingClientRect()
    const margin = 8
    const vw = window.innerWidth
    const vh = window.innerHeight
    let nx = anchor.x
    let ny = anchor.y
    if (nx + rect.width > vw - margin) nx = Math.max(margin, vw - rect.width - margin)
    if (ny + rect.height > vh - margin) ny = Math.max(margin, vh - rect.height - margin)
    if (nx !== pos.x || ny !== pos.y) setPos({ x: nx, y: ny })
  }, [anchor.x, anchor.y, pos.x, pos.y])

  if (visibleItems.length === 0) return null

  return (
    <div ref={ref} className="context-menu" role="menu" style={{ top: pos.y, left: pos.x }}>
      {visibleItems.map((item, idx) => (
        <button
          key={idx}
          role="menuitem"
          type="button"
          disabled={item.disabled}
          className={`context-menu-item${item.danger ? ' context-menu-danger' : ''}`}
          onClick={(e) => {
            e.stopPropagation()
            onClose()
            item.onSelect()
          }}
        >
          {item.label}
        </button>
      ))}
    </div>
  )
}
