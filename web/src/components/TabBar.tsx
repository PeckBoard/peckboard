import { useRef, useState } from 'react'
import { useTabsStore } from '../store/tabs'
import { useContextMenu } from '../hooks/useContextMenu'
import { type MenuItem } from './Dropdown'
import { tabDefaultLabel, type TabKindRegistry } from './tabKinds'

interface TabBarProps {
  /** Per-tab-kind glue. The TabBar is kind-agnostic; everything it
   *  needs to render and dispatch a tab comes from this registry,
   *  which the parent (App.tsx) builds from its stores. Adding a new
   *  tab kind = adding a new entry here — no TabBar changes needed. */
  kinds: TabKindRegistry
  /** Open the New Session modal. Renders as a trailing `+` button. */
  onNewSession: () => void
}

/**
 * Top tab strip. Persists server-side via `useTabsStore` so the same
 * set shows up on every device. The Sessions / Projects list entries
 * live in the navigation rail — keeping them out of here means the
 * strip can use all of its horizontal space for tabs, which matters on
 * mobile where the rail is the bottom toolbar.
 *
 * Kind-agnostic: every per-kind decision (label, badges, icon, menu,
 * navigation) is delegated to the [[TabKindRegistry]] passed in by the
 * parent. The TabBar's only job is layout, the long-press / right-click
 * context menu glue, the close affordance, and reordering.
 *
 * Close UX:
 *   Desktop: an X button on each tab (visible on hover/active); also
 *     right-click → context menu with Close tab + the kind's items.
 *   Mobile: the X button stays visible (no hover to reveal it) and
 *     long-press opens the same context menu.
 *   Closing the active tab clears the kind's active id and navigates
 *   to the list view (via `kind.onClose`) so App.tsx's open-on-active
 *   effect can't immediately re-open the tab from the stale URL.
 *
 * Reorder UX:
 *   Desktop: drag a chip onto another to drop it into that slot (HTML5
 *     drag-and-drop).
 *   Everywhere (incl. touch, where native drag doesn't fire): the
 *     context menu's Move left / Move right entries.
 */
export default function TabBar({ kinds, onNewSession }: TabBarProps) {
  const tabs = useTabsStore((s) => s.tabs)
  const closeTab = useTabsStore((s) => s.closeTab)
  const moveTab = useTabsStore((s) => s.moveTab)

  // Index of the chip currently being dragged. A ref (not state)
  // because it only needs to survive from dragstart to drop and
  // shouldn't trigger re-renders; `dragOver` *is* state so the drop
  // target can show an insertion cue.
  const dragFrom = useRef<number | null>(null)
  const [dragOver, setDragOver] = useState<number | null>(null)

  const handleDrop = (to: number) => {
    const from = dragFrom.current
    dragFrom.current = null
    setDragOver(null)
    if (from !== null) moveTab(from, to)
  }

  // Always render the strip — even with zero tabs — so the trailing `+`
  // button stays reachable as the user's entry point to creating a new
  // session.
  return (
    <div className="tabbar" role="tablist" aria-label="Open tabs">
      {tabs.map((t, i) => {
        const kind = kinds[t.itemType]
        if (!kind) return null
        const live = kind.getLiveName(t)
        // `||` (not `??`) is intentional: openTab's optimistic insert
        // stores `name: ''` for the brief window between local insert
        // and the upsert response landing, and the empty string must
        // fall through to the placeholder rather than render as a
        // label-less chip. Same reason `live` falls back to `t.name`.
        const label = live || t.name || tabDefaultLabel[t.itemType]
        const active = kind.isActive(t)
        const badges = kind.getBadges(t, active)
        const closeTitle = kind.getCloseTitle?.(t) ?? 'Close tab'
        // Reorder without a pointer drag — the only way to reorder on
        // touch. Layered before the kind's items; "Close tab" is
        // prepended inside OpenedTab.
        const reorderItems: MenuItem[] = [
          { label: 'Move left', onSelect: () => moveTab(i, i - 1), disabled: i === 0 },
          {
            label: 'Move right',
            onSelect: () => moveTab(i, i + 1),
            disabled: i === tabs.length - 1,
          },
        ]
        return (
          <OpenedTab
            key={`${t.itemType}:${t.itemId}`}
            label={label}
            active={active}
            closeTitle={closeTitle}
            running={badges.running}
            unread={badges.unread}
            icon={kind.getIcon(t)}
            menuItems={[...reorderItems, ...kind.getMenuItems(t)]}
            tabKey={`${t.itemType}:${t.itemId}`}
            dragOver={dragOver === i}
            onClick={() => kind.onActivate(t)}
            onClose={() => {
              // Clear the active id first (navigate away) so the stale
              // URL can't re-open the tab, then drop it from the strip.
              kind.onClose(t)
              closeTab(t.itemType, t.itemId)
            }}
            onDragStart={() => {
              dragFrom.current = i
            }}
            onDragEnter={() => {
              if (dragFrom.current !== null) setDragOver(i)
            }}
            onDrop={() => handleDrop(i)}
            onDragEnd={() => {
              dragFrom.current = null
              setDragOver(null)
            }}
          />
        )
      })}
      <button
        type="button"
        className="tab-new"
        title="New session"
        aria-label="New session"
        onClick={onNewSession}
      >
        +
      </button>
    </div>
  )
}

interface OpenedTabProps {
  label: string
  closeTitle: string
  active: boolean
  running: boolean
  unread: boolean
  icon: React.ReactNode
  menuItems: MenuItem[]
  tabKey: string
  dragOver: boolean
  onClick: () => void
  onClose: () => void
  onDragStart: () => void
  onDragEnter: () => void
  onDrop: () => void
  onDragEnd: () => void
}

function OpenedTab({
  label,
  closeTitle,
  active,
  running,
  unread,
  icon,
  menuItems,
  tabKey,
  dragOver,
  onClick,
  onClose,
  onDragStart,
  onDragEnter,
  onDrop,
  onDragEnd,
}: OpenedTabProps) {
  // The right-click / long-press menu always starts with "Close tab" —
  // it's tab-strip chrome, not a property of the underlying item.
  // Per CLAUDE.md "Component Reuse", the per-kind items below mirror
  // the closest 3-dot menu surface (sessions match ChatView, etc.).
  const items: MenuItem[] = [{ label: 'Close tab', onSelect: onClose }, ...menuItems]
  const { triggerProps, menu, consumeLongPressClick } = useContextMenu(() =>
    items
      .filter((m) => !m.divider && !m.hidden)
      .map((m) => ({
        label: m.label ?? '',
        onSelect: () => m.onSelect?.(),
        danger: m.danger,
        disabled: m.disabled,
      })),
  )

  return (
    <div
      className={`tab-wrap ${dragOver ? 'tab-dragover' : ''}`}
      data-tab-id={tabKey}
      draggable
      onDragStart={(e) => {
        // Firefox refuses to start a drag unless data is set; the
        // payload is unused (the dragged index lives in TabBar).
        e.dataTransfer.effectAllowed = 'move'
        e.dataTransfer.setData('text/plain', tabKey)
        onDragStart()
      }}
      onDragEnter={onDragEnter}
      onDragOver={(e) => e.preventDefault()}
      onDrop={(e) => {
        e.preventDefault()
        onDrop()
      }}
      onDragEnd={onDragEnd}
    >
      <button
        role="tab"
        aria-selected={active}
        className={`tab tab-opened ${active ? 'tab-active' : ''}`}
        onClick={(e) => {
          if (consumeLongPressClick(e)) return
          onClick()
        }}
        {...triggerProps}
      >
        {icon}
        {running ? (
          <span className="tab-dot tab-dot-running" aria-label="running" />
        ) : unread ? (
          <span className="tab-dot tab-dot-unread" aria-label="unread" />
        ) : null}
        <span className="tab-label">{label}</span>
      </button>
      <button
        className="tab-close"
        aria-label={`Close ${label}`}
        title={closeTitle}
        onClick={(e) => {
          e.stopPropagation()
          onClose()
        }}
      >
        &#10005;
      </button>
      {menu}
    </div>
  )
}
