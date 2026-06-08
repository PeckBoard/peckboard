import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react'
import { createPortal } from 'react-dom'
import ContextMenuView, { type ContextMenuItem } from '../components/ContextMenuView'

export type { ContextMenuItem }

interface Anchor {
  x: number
  y: number
}

interface TriggerProps {
  onContextMenu: (e: React.MouseEvent) => void
  onTouchStart: (e: React.TouchEvent) => void
  onTouchEnd: () => void
  onTouchCancel: () => void
  onTouchMove: () => void
}

interface UseContextMenuResult {
  /** Spread onto the trigger element to wire right-click + long-press. */
  triggerProps: TriggerProps
  /** Portal node to render somewhere in the tree (typically right after
   *  the trigger). `null` when the menu is closed. */
  menu: ReactNode
  /** Call from the trigger's onClick: returns true and swallows the
   *  event if the click is actually the synthetic one fired right after
   *  a long-press, which would otherwise activate the underlying button
   *  the instant the menu opens. */
  consumeLongPressClick: (e: React.SyntheticEvent) => boolean
}

const LONG_PRESS_MS = 450

/**
 * Reusable context-menu primitive. Wire it onto any element via
 * `triggerProps` for right-click (desktop) and long-press (touch); the
 * `menu` node renders the popup in a portal anchored to the viewport,
 * so it never gets clipped by an ancestor's `overflow: hidden` or
 * scrolling container.
 *
 * `buildItems` is called each render the menu is open so callers can
 * close over fresh props without memoisation gymnastics.
 */
export function useContextMenu(buildItems: () => ContextMenuItem[]): UseContextMenuResult {
  const [anchor, setAnchor] = useState<Anchor | null>(null)
  const longPressTimer = useRef<number | undefined>(undefined)
  const longPressFired = useRef(false)

  const close = useCallback(() => setAnchor(null), [])

  // Outside-click + Escape dismissal. `mousedown` (not `click`) so a
  // left-click outside dismisses the menu before the click lands on
  // whatever it was pointing at — matches native OS context-menu UX.
  useEffect(() => {
    if (!anchor) return
    const onDown = (e: MouseEvent) => {
      const target = e.target as HTMLElement | null
      if (target?.closest('.context-menu')) return
      setAnchor(null)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setAnchor(null)
    }
    document.addEventListener('mousedown', onDown)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDown)
      document.removeEventListener('keydown', onKey)
    }
  }, [anchor])

  const startLongPress = useCallback((e: React.TouchEvent) => {
    const touch = e.touches[0]
    if (!touch) return
    const x = touch.clientX
    const y = touch.clientY
    longPressFired.current = false
    longPressTimer.current = window.setTimeout(() => {
      longPressFired.current = true
      setAnchor({ x, y })
    }, LONG_PRESS_MS)
  }, [])
  const cancelLongPress = useCallback(() => {
    if (longPressTimer.current !== undefined) {
      window.clearTimeout(longPressTimer.current)
      longPressTimer.current = undefined
    }
  }, [])

  const triggerProps: TriggerProps = {
    onContextMenu: (e) => {
      e.preventDefault()
      setAnchor({ x: e.clientX, y: e.clientY })
    },
    onTouchStart: startLongPress,
    onTouchEnd: cancelLongPress,
    onTouchCancel: cancelLongPress,
    onTouchMove: cancelLongPress,
  }

  const consumeLongPressClick = (e: React.SyntheticEvent) => {
    if (longPressFired.current) {
      e.preventDefault()
      e.stopPropagation()
      longPressFired.current = false
      return true
    }
    return false
  }

  const menu = anchor
    ? createPortal(
        <ContextMenuView anchor={anchor} items={buildItems()} onClose={close} />,
        document.body,
      )
    : null

  return { triggerProps, menu, consumeLongPressClick }
}
