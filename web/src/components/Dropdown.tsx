import {
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from 'react'
import { createPortal } from 'react-dom'
import { focusFirstMenuItem, handleMenuKeys } from '../hooks/useMenuKeyboard'

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
  /** Minimum popup width in px. The model picker uses it to keep the popup
   *  at least as wide as the trigger it hangs off. */
  minWidth?: number
  /** Testid for the searchable variant's filter input. */
  searchTestId?: string
  /** Row shown when the list is empty BEFORE filtering (e.g. "Loading
   *  models…"). A filter that matches nothing always says "No matches". */
  emptyLabel?: string
  /** Accessible name for the searchable variant's option list. */
  listLabel?: string
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
  minWidth,
  searchTestId,
  emptyLabel,
  listLabel,
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
  // Ids for the searchable (listbox) variant: the filter input is the
  // combobox, the rows are its options, and `aria-activedescendant` points
  // at the keyboard cursor — so a screen reader follows Arrow keys while
  // focus stays in the input where the user is typing.
  // (`useId` hands back `:r3:`-shaped strings; the colons are stripped so
  // the ids are usable in a CSS selector.)
  const baseId = useId().replace(/:/g, '')
  const listId = `${baseId}-list`
  const activeOptionId = selectable.length > 0 ? `${baseId}-opt-${highlight}` : undefined

  // Keep the keyboard cursor in view when arrowing through a long list.
  useEffect(() => {
    if (!searchable) return
    ref.current?.querySelector('.model-picker-item-highlight')?.scrollIntoView({ block: 'nearest' })
  }, [searchable, highlight, query])

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
    // Functional update, and `pos` deliberately NOT in the deps: a fixed
    // element with no explicit width shrink-to-fits against the viewport
    // edge, so getBoundingClientRect() can return a (fractionally) different
    // width after every reposition — feeding `pos` back into the effect then
    // oscillates forever and trips React's update-depth limit (#185).
    setPos((p) => (p.left === left && p.top === top ? p : { left, top }))
  }, [anchor.x, anchor.y, align])

  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      const target = e.target as HTMLElement | null
      if (target?.closest('.dropdown-menu')) return
      onClose()
    }
    // Escape closes only this popup. Capture phase + stopPropagation is
    // deliberate: a picker opened from inside a Modal must swallow the key,
    // otherwise the dialog's own document-level Escape handler fires on the
    // same keystroke and the user loses the whole form behind the popup.
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return
      e.stopPropagation()
      e.preventDefault()
      onClose()
    }
    document.addEventListener('mousedown', onDown)
    document.addEventListener('keydown', onKey, true)
    return () => {
      document.removeEventListener('mousedown', onDown)
      document.removeEventListener('keydown', onKey, true)
    }
  }, [onClose])

  // Roving keyboard focus for the plain (non-searchable) menu, via the
  // shared menu model in `hooks/useMenuKeyboard`. `role="menu"` promises
  // arrow navigation; without it the only way through a popup is Tab, and a
  // keyboard-only user can never reach a submenu. The searchable variant
  // keeps its own input-driven highlight model (`onSearchKey`).
  useEffect(() => {
    if (searchable) return
    focusFirstMenuItem(ref.current)
  }, [searchable])

  const onMenuKey = (e: React.KeyboardEvent) => {
    if (searchable) return
    if (handleMenuKeys(e, ref.current)) return
    if (e.key === 'ArrowRight') {
      // Open a submenu row; the flyout focuses its own first item on mount.
      const el = document.activeElement as HTMLElement | null
      if (el?.classList.contains('dropdown-item-has-sub') && ref.current?.contains(el)) {
        e.preventDefault()
        el.click()
      }
    }
  }

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
        listbox={!!searchable}
        id={searchable && isSelectable ? `${baseId}-opt-${at}` : undefined}
        highlighted={searchable && isSelectable && at === highlight}
        onHover={searchable && isSelectable ? () => setHighlight(at) : undefined}
      />
    )
  })

  return createPortal(
    <div
      ref={ref}
      className={`dropdown-menu${searchable ? ' model-picker-popup' : ''}${className ? ` ${className}` : ''}`}
      // The searchable variant is a combobox + listbox, not a menu: mixing
      // `role="menu"` with `role="option"` rows is invalid, and the listbox
      // model is the one ModelPicker proved out.
      role={searchable ? undefined : 'menu'}
      onKeyDown={onMenuKey}
      style={{
        position: 'fixed',
        left: pos.left,
        top: pos.top,
        minWidth,
        maxWidth: `calc(100vw - ${MENU_MARGIN * 2}px)`,
      }}
    >
      {searchable ? (
        <>
          <input
            className="model-picker-search"
            type="text"
            role="combobox"
            autoFocus
            value={query}
            placeholder={searchPlaceholder ?? 'Search…'}
            aria-label={searchPlaceholder ?? 'Search'}
            aria-expanded="true"
            aria-controls={listId}
            aria-activedescendant={activeOptionId}
            aria-autocomplete="list"
            data-testid={searchTestId}
            onChange={(e) => {
              setQuery(e.target.value)
              setHighlight(0)
            }}
            onKeyDown={onSearchKey}
          />
          <div
            className="model-picker-list"
            id={listId}
            role="listbox"
            aria-label={listLabel ?? searchPlaceholder ?? 'Options'}
          >
            {rows.length > 0 ? (
              rows
            ) : (
              <button type="button" className="dropdown-item" disabled>
                {visible.length === 0 && emptyLabel ? emptyLabel : 'No matches'}
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
  listbox,
  id,
  highlighted,
  onHover,
}: {
  item: MenuItem
  onClose: () => void
  /** Render as a listbox `option` rather than a `menuitem` — the searchable
   *  variant is a combobox, so its rows carry option semantics. */
  listbox?: boolean
  /** Element id, referenced by the combobox's `aria-activedescendant`. */
  id?: string
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
          data-menu-active={item.active ? 'true' : undefined}
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
      id={id}
      role={listbox ? 'option' : 'menuitem'}
      aria-selected={listbox ? !!item.active : undefined}
      type="button"
      className={`dropdown-item${item.danger ? ' dropdown-item-danger' : ''}${item.active ? ' dropdown-item-active' : ''}${item.description ? ' dropdown-item-with-desc' : ''}${highlighted ? ' model-picker-item-highlight' : ''}`}
      disabled={item.disabled}
      onMouseEnter={onHover}
      onClick={(e) => {
        e.stopPropagation()
        onClose()
        item.onSelect?.()
      }}
      data-menu-active={item.active ? 'true' : undefined}
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
  /** Testid for the searchable popup's filter input. */
  searchTestId?: string
  /** Row shown when `items` is empty before filtering (e.g. "Loading models…"). */
  emptyLabel?: string
  /** Accessible name for the searchable popup's option list. */
  listLabel?: string
  /** What the trigger announces it opens: `listbox` for a single-choice
   *  searchable picker, `menu` (the default) for an action menu. */
  haspopup?: 'menu' | 'listbox'
  /** Open the popup at least as wide as the trigger. */
  matchTriggerWidth?: boolean
  /** Floor for the popup width in px (combines with `matchTriggerWidth`). */
  minWidth?: number
  /** Id forwarded to the trigger, so a `<label htmlFor>` can point at it. */
  id?: string
  /** Disable the trigger. */
  disabled?: boolean
  /** Fired when the popup opens — e.g. to (re)fetch the list it shows. */
  onOpen?: () => void
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
  searchTestId,
  emptyLabel,
  listLabel,
  haspopup = 'menu',
  matchTriggerWidth,
  minWidth,
  id,
  disabled,
  onOpen,
  children,
}: MenuButtonProps) {
  const [anchor, setAnchor] = useState<{ x: number; y: number } | null>(null)
  const [triggerWidth, setTriggerWidth] = useState(0)
  const triggerRef = useRef<HTMLButtonElement | null>(null)
  // Closing hands focus back to the trigger: the popup is portalled to the
  // end of <body>, so leaving focus there would drop a keyboard user at the
  // top of the document on the next Tab. An outside click that focused
  // something else keeps its focus.
  const close = useCallback(() => {
    setAnchor(null)
    const active = document.activeElement as HTMLElement | null
    if (!active || active === document.body || active.closest('.dropdown-menu')) {
      triggerRef.current?.focus()
    }
  }, [])

  const openAt = (el: HTMLButtonElement) => {
    const r = el.getBoundingClientRect()
    if (matchTriggerWidth) setTriggerWidth(r.width)
    setAnchor({ x: align === 'right' ? r.right : r.left, y: r.bottom + 4 })
    onOpen?.()
  }

  const onClick = (e: React.MouseEvent<HTMLButtonElement>) => {
    e.stopPropagation()
    if (anchor) {
      setAnchor(null)
      return
    }
    openAt(e.currentTarget)
  }

  // ArrowDown/ArrowUp open the menu from the trigger; Enter/Space are the
  // button's native click. Either way the popup focuses its first item.
  const onKeyDown = (e: React.KeyboardEvent<HTMLButtonElement>) => {
    if (!anchor && (e.key === 'ArrowDown' || e.key === 'ArrowUp')) {
      e.preventDefault()
      openAt(e.currentTarget)
    }
  }

  return (
    <>
      <button
        ref={triggerRef}
        type="button"
        id={id}
        className={triggerClassName ?? 'menu-button'}
        aria-label={ariaLabel}
        aria-haspopup={haspopup}
        aria-expanded={!!anchor}
        disabled={disabled}
        title={title}
        data-testid={testId}
        onClick={onClick}
        onKeyDown={onKeyDown}
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
          searchTestId={searchTestId}
          emptyLabel={emptyLabel}
          listLabel={listLabel}
          minWidth={matchTriggerWidth ? Math.max(triggerWidth, minWidth ?? 0) : minWidth}
        />
      )}
    </>
  )
}
