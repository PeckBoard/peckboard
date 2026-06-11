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
 * context menu glue, and the close affordance.
 *
 * Close UX:
 *   Desktop: an X button on each tab (visible on hover/active); also
 *     right-click → context menu with Close tab + the kind's items.
 *   Mobile: long-press → the same context menu. The X is hidden under
 *     the 768px breakpoint to keep tab chips compact.
 */
export default function TabBar({ kinds, onNewSession }: TabBarProps) {
  const tabs = useTabsStore((s) => s.tabs)
  const closeTab = useTabsStore((s) => s.closeTab)

  // No frontend cleanup loop: the server-side `list_tabs` handler in
  // src/routes/me.rs filters out tabs whose underlying item is gone,
  // and explicit deletes call `removeTabsForItem` directly.

  // Always render the strip — even with zero tabs — so the trailing `+`
  // button stays reachable as the user's entry point to creating a new
  // session.
  return (
    <div className="tabbar" role="tablist" aria-label="Open tabs">
      {tabs.map((t) => {
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
        return (
          <OpenedTab
            key={`${t.itemType}:${t.itemId}`}
            label={label}
            active={active}
            running={badges.running}
            unread={badges.unread}
            icon={kind.getIcon(t)}
            menuItems={kind.getMenuItems(t)}
            tabKey={`${t.itemType}:${t.itemId}`}
            onClick={() => kind.onActivate(t)}
            onClose={() => closeTab(t.itemType, t.itemId)}
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
  active: boolean
  running: boolean
  unread: boolean
  icon: React.ReactNode
  menuItems: MenuItem[]
  tabKey: string
  onClick: () => void
  onClose: () => void
}

function OpenedTab({
  label,
  active,
  running,
  unread,
  icon,
  menuItems,
  tabKey,
  onClick,
  onClose,
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
    <div className="tab-wrap" data-tab-id={tabKey}>
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
        title="Close tab"
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
