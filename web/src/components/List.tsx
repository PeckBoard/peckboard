import type { ReactNode } from 'react'
import { useContextMenu } from '../hooks/useContextMenu'
import { MenuButton, type MenuItem } from './Dropdown'

export interface ListAction {
  label: string
  onClick: () => void
  danger?: boolean
  /** Hide an action without splitting the call site. */
  hidden?: boolean
}

interface ListProps<T> {
  items: T[]
  getKey: (item: T) => string
  /** Inline content for the clickable item button (icons, name, meta). */
  renderItem: (item: T) => ReactNode
  /** Per-row menu items used by BOTH the 3-dot button AND right-click /
   *  long-press. Pass an empty array (or omit) to hide the menu entirely. */
  getMenuItems?: (item: T) => MenuItem[]
  /** Active item id — gets the `.active` style. */
  activeId?: string | null
  onActivate: (item: T) => void
  /** Set of selected item ids. When provided, each row renders a checkbox
   *  and the `selected` style. Omit to disable multi-select. */
  selectedIds?: Set<string>
  onToggleSelected?: (item: T) => void
  /** Bulk actions shown in the floating action bar when one or more rows
   *  are selected. */
  bulkActions?: ListAction[]
  onClearSelection?: () => void
  /** Optional extra class on the body container — defaults to
   *  `.list-view-body`. */
  bodyClassName?: string
  /** Optional scroll handler — used for paginated infinite scroll. */
  onScroll?: (e: React.UIEvent<HTMLDivElement>) => void
  /** Slot rendered when `items` is empty. */
  emptyState?: ReactNode
  /** Slot rendered at the bottom of the body, after the rows. */
  footer?: ReactNode
}

/**
 * Shared list component used by every "list of things" view in the app —
 * sessions, projects, repeating tasks, etc. Owns the row chrome, the 3-dot
 * menu, the right-click / long-press context menu, multi-select selection,
 * and the floating bulk-action bar.
 *
 * Per CLAUDE.md "Component Reuse", new list-style views MUST use this
 * component rather than re-rolling rows. The visual chrome (`.list-view-*`
 * classes) is shared too.
 */
export default function List<T>({
  items,
  getKey,
  renderItem,
  getMenuItems,
  activeId,
  onActivate,
  selectedIds,
  onToggleSelected,
  bulkActions,
  onClearSelection,
  bodyClassName,
  onScroll,
  emptyState,
  footer,
}: ListProps<T>) {
  const selectable = !!selectedIds && !!onToggleSelected
  const selCount = selectedIds?.size ?? 0
  const visibleBulkActions = (bulkActions ?? []).filter((a) => !a.hidden)
  return (
    <>
      {selectable && selCount > 0 && (
        <div className="bulk-action-bar" data-testid="bulk-action-bar">
          <span className="bulk-action-count">{selCount} selected</span>
          <div className="bulk-action-buttons">
            {visibleBulkActions.map((a, i) => (
              <button
                key={i}
                type="button"
                className={`bulk-action-btn${a.danger ? ' danger' : ''}`}
                onClick={a.onClick}
              >
                {a.label}
              </button>
            ))}
            {onClearSelection && (
              <button type="button" className="bulk-action-btn" onClick={onClearSelection}>
                Clear
              </button>
            )}
          </div>
        </div>
      )}
      <div className={bodyClassName ?? 'list-view-body'} onScroll={onScroll}>
        {items.length === 0
          ? emptyState
          : items.map((item) => {
              const key = getKey(item)
              const isActive = activeId === key
              const isSelected = selectedIds?.has(key) ?? false
              const menuItems = getMenuItems?.(item) ?? []
              return (
                <ListRow
                  key={key}
                  rowKey={key}
                  isActive={isActive}
                  isSelected={isSelected}
                  selectable={selectable}
                  onToggleSelected={() => onToggleSelected?.(item)}
                  onActivate={() => onActivate(item)}
                  menuItems={menuItems}
                >
                  {renderItem(item)}
                </ListRow>
              )
            })}
        {footer}
      </div>
    </>
  )
}

function ListRow({
  rowKey,
  isActive,
  isSelected,
  selectable,
  onToggleSelected,
  onActivate,
  menuItems,
  children,
}: {
  rowKey: string
  isActive: boolean
  isSelected: boolean
  selectable: boolean
  onToggleSelected: () => void
  onActivate: () => void
  menuItems: MenuItem[]
  children: ReactNode
}) {
  // Right-click / long-press menu mirrors the 3-dot menu — same items, same
  // labels, same order. That guarantee is what makes the menus feel unified.
  const { triggerProps, menu, consumeLongPressClick } = useContextMenu(() =>
    menuItems
      .filter((m) => !m.divider && !m.hidden)
      .map((m) => ({
        label: m.label ?? '',
        onSelect: () => m.onSelect?.(),
        danger: m.danger,
        disabled: m.disabled,
      })),
  )
  const hasMenu = menuItems.some((m) => !m.divider && !m.hidden)
  const className = `list-view-row${isActive ? ' active' : ''}${isSelected ? ' selected' : ''}`
  return (
    <div className={className} data-row-id={rowKey} {...triggerProps}>
      {selectable && (
        <input
          type="checkbox"
          className="list-view-select"
          checked={isSelected}
          onClick={(e) => e.stopPropagation()}
          onChange={onToggleSelected}
          aria-label="Select row"
        />
      )}
      <button
        type="button"
        className="list-view-item"
        onClick={(e) => {
          if (consumeLongPressClick(e)) return
          onActivate()
        }}
      >
        {children}
      </button>
      {hasMenu && (
        <MenuButton
          items={menuItems}
          ariaLabel="Row menu"
          triggerClassName="list-view-menu"
          align="right"
        >
          <span aria-hidden="true">···</span>
        </MenuButton>
      )}
      {menu}
    </div>
  )
}
