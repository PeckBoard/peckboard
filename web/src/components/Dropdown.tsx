import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import { createPortal } from 'react-dom'

/**
 * Shared menu-item shape used by every dropdown / context menu / 3-dot
 * popup in the app. New menus should NOT invent their own item shape —
 * see CLAUDE.md "Component Reuse" for the rule.
 *
 * `divider: true` items render as a horizontal rule; their other fields
 * are ignored. `submenu` items render an expandable sub-popup so a
 * single menu can group secondary actions (e.g. Model: > pick model).
 */
export interface MenuItem {
  /** Display text. Ignored when `divider` is true. */
  label?: string
  /** Optional trailing hint shown muted on the right (e.g. current value). */
  hint?: string
  /** Optional secondary line rendered muted UNDER the label — used for
   *  pickers where the value alone (workflow, model variant, etc.) isn't
   *  self-describing and a one-line summary helps the user choose. */
  description?: string
  /** Invoked when the user clicks the item. Mutually exclusive with `submenu`. */
  onSelect?: () => void
  /** Submenu items. When set, the row opens a flyout instead of invoking. */
  submenu?: MenuItem[]
  /** Render in the danger style (red). */
  danger?: boolean
  /** Greyed out and non-interactive. */
  disabled?: boolean
  /** Mark the currently-active option (used in single-choice submenus). */
  active?: boolean
  /** Render as a horizontal divider. All other fields are ignored. */
  divider?: boolean
  /** Skip rendering. Lets callers express "this action only applies to
   *  sessions" without splitting the list. */
  hidden?: boolean
  /** Extra text matched by a searchable submenu's filter, never displayed.
   *  E.g. a model's full id, which carries the provider and account. */
  searchText?: string
  /** With `submenu`: render a filter input at the top of the flyout and make
   *  the list scrollable. For long single-choice lists (model catalogue). */
  searchable?: boolean
  /** Placeholder for the searchable flyout's filter input. */
  searchPlaceholder?: string
  /** Optional testid forwarded to the rendered button. */
  testId?: string
}

interface DropdownProps {
  /** Viewport anchor point for the menu (e.g. trigger button's bottom-right). */
  anchor: { x: number; y: number }
  items: MenuItem[]
  /** Called when the user dismisses (click outside, Escape, item select). */
  onClose: () => void
  /** Preferred horizontal alignment — anchor `x` is treated as either the
   *  right or left edge of the menu. Defaults to right (menu opens leftward
   *  from the anchor, matching a 3-dot button on the right of a row). */
  align?: 'left' | 'right'
  /** Optional class for the popup, for one-off layout overrides. */
  className?: string
  /** Render a filter input above the items; rows are matched on label, hint,
   *  and `searchText`. Set via `searchable: true` on a submenu item. */
  searchable?: boolean
  searchPlaceholder?: string
}

const MENU_MARGIN = 8

/**
 * Portal-rendered popup menu. The single dropdown primitive used by every
 * 3-dot menu, model picker, and click-anchored popup in the app. Right-click
 * menus go through the `useContextMenu` hook, which composes the same
 * `MenuItem` list — keep the shape compatible.
 */
export default function Dropdown({
  anchor,
  items,
  onClose,
  align = 'right',
  className,
  searchable,
  searchPlaceholder,
}: DropdownProps) {
  const ref = useRef<HTMLDivElement | null>(null)
  const [pos, setPos] = useState<{ left: number; top: number }>(() => ({
    left: align === 'right' ? anchor.x : anchor.x,
    top: anchor.y,
  }))
  const visible = items.filter((i) => !i.hidden)
  // Searchable variant: filter rows by the query and track a keyboard cursor
  // over the selectable rows so ArrowUp/Down + Enter work from the input
  // (mirrors ModelPicker's interaction).
  const [query, setQuery] = useState('')
  const q = query.trim().toLowerCase()
  const filtered = q
    ? visible.filter(
        (i) =>
          !i.divider &&
          `${i.label ?? ''} ${i.hint ?? ''} ${i.searchText ?? ''}`.toLowerCase().includes(q),
      )
    : visible
  const selectable = filtered.filter((i) => !i.divider && !i.disabled && i.onSelect)
  const [highlight, setHighlight] = useState(() =>
    Math.max(
      0,
      selectable.findIndex((i) => i.active),
    ),
  )

  useLayoutEffect(() => {
    const el = ref.current
    if (!el) return
    const rect = el.getBoundingClientRect()
    const vw = window.innerWidth
    const vh = window.innerHeight
    let left = align === 'right' ? anchor.x - rect.width : anchor.x
    let top = anchor.y
    if (left + rect.width > vw - MENU_MARGIN) left = vw - rect.width - MENU_MARGIN
    if (left < MENU_MARGIN) left = MENU_MARGIN
    if (top + rect.height > vh - MENU_MARGIN)
      top = Math.max(MENU_MARGIN, vh - rect.height - MENU_MARGIN)
    if (left !== pos.left || top !== pos.top) setPos({ left, top })
  }, [anchor.x, anchor.y, align, pos.left, pos.top])

  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      const target = e.target as HTMLElement | null
      if (target?.closest('.dropdown-menu')) return
      onClose()
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
    }
    document.addEventListener('mousedown', onDown)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDown)
      document.removeEventListener('keydown', onKey)
    }
  }, [onClose])

  const onSearchKey = (e: React.KeyboardEvent) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault()
      setHighlight((h) => Math.min(h + 1, selectable.length - 1))
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      setHighlight((h) => Math.max(h - 1, 0))
    } else if (e.key === 'Enter') {
      e.preventDefault()
      const item = selectable[highlight]
      if (item) {
        onClose()
        item.onSelect?.()
      }
    }
  }

  if (!searchable && visible.length === 0) return null

  let selIdx = -1
  const rows = filtered.map((item, idx) => {
    const isSelectable = !item.divider && !item.disabled && !!item.onSelect
    if (isSelectable) selIdx++
    const at = selIdx
    return (
      <MenuRow
        key={idx}
        item={item}
        onClose={onClose}
        highlighted={searchable && isSelectable && at === highlight}
        onHover={searchable && isSelectable ? () => setHighlight(at) : undefined}
      />
    )
  })

  return createPortal(
    <div
      ref={ref}
      className={`dropdown-menu${searchable ? ' model-picker-popup' : ''}${className ? ` ${className}` : ''}`}
      role="menu"
      style={{ position: 'fixed', left: pos.left, top: pos.top }}
    >
      {searchable ? (
        <>
          <input
            className="model-picker-search"
            type="text"
            autoFocus
            value={query}
            placeholder={searchPlaceholder ?? 'Search…'}
            aria-label={searchPlaceholder ?? 'Search'}
            onChange={(e) => {
              setQuery(e.target.value)
              setHighlight(0)
            }}
            onKeyDown={onSearchKey}
          />
          <div className="model-picker-list">
            {rows.length > 0 ? (
              rows
            ) : (
              <button type="button" className="dropdown-item" disabled>
                No matches
              </button>
            )}
          </div>
        </>
      ) : (
        rows
      )}
    </div>,
    document.body,
  )
}

function MenuRow({
  item,
  onClose,
  highlighted,
  onHover,
}: {
  item: MenuItem
  onClose: () => void
  /** Keyboard cursor from a searchable parent — visual only. */
  highlighted?: boolean
  /** Sync the keyboard cursor when the mouse moves over this row. */
  onHover?: () => void
}) {
  const [subAnchor, setSubAnchor] = useState<{ x: number; y: number } | null>(null)
  const btnRef = useRef<HTMLButtonElement | null>(null)

  if (item.divider) return <div className="dropdown-divider" role="separator" />

  if (item.submenu && item.submenu.length > 0) {
    const open = () => {
      const el = btnRef.current
      if (!el) return
      const r = el.getBoundingClientRect()
      setSubAnchor({ x: r.right, y: r.top })
    }
    return (
      <>
        <button
          ref={btnRef}
          role="menuitem"
          type="button"
          className={`dropdown-item dropdown-item-has-sub${item.danger ? ' dropdown-item-danger' : ''}${item.active ? ' dropdown-item-active' : ''}`}
          disabled={item.disabled}
          onClick={(e) => {
            e.stopPropagation()
            open()
          }}
          data-testid={item.testId}
        >
          <span className="dropdown-item-label">{item.label}</span>
          {item.hint && <span className="dropdown-item-hint">{item.hint}</span>}
          <span className="dropdown-item-chev" aria-hidden="true">
            &rsaquo;
          </span>
        </button>
        {subAnchor && (
          <Dropdown
            anchor={subAnchor}
            items={item.submenu}
            onClose={() => {
              setSubAnchor(null)
              onClose()
            }}
            searchable={item.searchable}
            searchPlaceholder={item.searchPlaceholder}
            align="left"
          />
        )}
      </>
    )
  }

  return (
    <button
      role="menuitem"
      type="button"
      className={`dropdown-item${item.danger ? ' dropdown-item-danger' : ''}${item.active ? ' dropdown-item-active' : ''}${item.description ? ' dropdown-item-with-desc' : ''}${highlighted ? ' model-picker-item-highlight' : ''}`}
      disabled={item.disabled}
      onMouseEnter={onHover}
      onClick={(e) => {
        e.stopPropagation()
        onClose()
        item.onSelect?.()
      }}
      data-testid={item.testId}
    >
      <span className="dropdown-item-row">
        <span className="dropdown-item-label">{item.label}</span>
        {item.hint && <span className="dropdown-item-hint">{item.hint}</span>}
      </span>
      {item.description && <span className="dropdown-item-desc">{item.description}</span>}
    </button>
  )
}

interface MenuButtonProps {
  /** Items to render in the popup. */
  items: MenuItem[]
  /** Accessible label for the trigger. */
  ariaLabel?: string
  /** Optional override for the trigger button class. Defaults to the
   *  shared `.menu-button` styling. */
  triggerClassName?: string
  /** Optional title attr for hover tooltip. */
  title?: string
  /** Optional testid on the trigger button. */
  testId?: string
  /** Optional alignment override. Defaults to 'right' (menu opens leftward). */
  align?: 'left' | 'right'
  /** Render a filter input above the popup items and make the list
   *  scrollable — same behaviour as a `searchable` submenu. */
  searchable?: boolean
  /** Placeholder for the searchable popup's filter input. */
  searchPlaceholder?: string
  /** Trigger glyph. Defaults to the 3-dot SVG. */
  children?: ReactNode
}

/**
 * The standard 3-dot / overflow trigger + Dropdown pair. Drop one of these
 * into a row, a card, or a toolbar wherever you previously hand-rolled an
 * overflow menu — see CLAUDE.md "Component Reuse".
 */
export function MenuButton({
  items,
  ariaLabel = 'Menu',
  triggerClassName,
  title,
  testId,
  align = 'right',
  searchable,
  searchPlaceholder,
  children,
}: MenuButtonProps) {
  const [anchor, setAnchor] = useState<{ x: number; y: number } | null>(null)
  const close = useCallback(() => setAnchor(null), [])

  const onClick = (e: React.MouseEvent<HTMLButtonElement>) => {
    e.stopPropagation()
    if (anchor) {
      setAnchor(null)
      return
    }
    const r = e.currentTarget.getBoundingClientRect()
    setAnchor({ x: align === 'right' ? r.right : r.left, y: r.bottom + 4 })
  }

  return (
    <>
      <button
        type="button"
        className={triggerClassName ?? 'menu-button'}
        aria-label={ariaLabel}
        title={title}
        data-testid={testId}
        onClick={onClick}
      >
        {children ?? (
          <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor" aria-hidden="true">
            <circle cx="8" cy="3" r="1.5" />
            <circle cx="8" cy="8" r="1.5" />
            <circle cx="8" cy="13" r="1.5" />
          </svg>
        )}
      </button>
      {anchor && (
        <Dropdown
          anchor={anchor}
          items={items}
          onClose={close}
          align={align}
          searchable={searchable}
          searchPlaceholder={searchPlaceholder}
        />
      )}
    </>
  )
}
