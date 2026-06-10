import { useMemo } from 'react'
import { useSessionsStore } from '../store/sessions'
import { useProjectsStore } from '../store/projects'
import { useTabsStore, type TabType } from '../store/tabs'
import { useContextMenu, type ContextMenuItem } from '../hooks/useContextMenu'

interface TabBarProps {
  view: 'sessions' | 'repeatingTasks' | 'projects' | 'experts' | 'folders' | 'reports' | 'users'
  activeSessionId: string | null
  activeProjectId: string | null
  onOpenItem: (type: TabType, id: string) => void
  onRenameItem: (type: TabType, id: string) => void
  /** Clear all messages in a session. Only invoked for `type === 'session'`. */
  onClearItem: (type: TabType, id: string) => void
  onDeleteItem: (type: TabType, id: string) => void
  /** Open the New Session modal. Renders as a trailing `+` button. */
  onNewSession: () => void
}

/**
 * Top tab strip showing the user's opened sessions and projects mixed
 * together in MRU order, persisted server-side via `useTabsStore` so
 * the same set shows up on every device. The Sessions / Projects list
 * entries live in the navigation rail — keeping them out of here means
 * the strip can use all of its horizontal space for tabs, which matters
 * on mobile where the rail is the bottom toolbar.
 *
 * Close UX:
 *   Desktop: an X button on each tab (visible on hover/active);
 *     also right-click → context menu with Close, Rename, Clear (sessions
 *     only), and Delete.
 *   Mobile:  long-press → the same context menu. The X is hidden under
 *     the 768px breakpoint to keep tab chips compact.
 */
export default function TabBar({
  view,
  activeSessionId,
  activeProjectId,
  onOpenItem,
  onRenameItem,
  onClearItem,
  onDeleteItem,
  onNewSession,
}: TabBarProps) {
  const tabs = useTabsStore((s) => s.tabs)
  const closeTab = useTabsStore((s) => s.closeTab)
  const sessions = useSessionsStore((s) => s.sessions)
  const projects = useProjectsStore((s) => s.projects)
  const unreadSessions = useSessionsStore((s) => s.unreadSessions)
  const processing = useSessionsStore((s) => s.processing)

  // Map by id for the live-state lookups (running/unread on session
  // tabs, project icon, etc.). Names themselves come from `t.name`
  // which the server denormalizes into the tab payload — that's what
  // lets us label worker-session tabs even though they're not in the
  // regular sessions list.
  const sessionMap = useMemo(() => new Map(sessions.map((s) => [s.id, s])), [sessions])
  const projectMap = useMemo(() => new Map(projects.map((p) => [p.id, p])), [projects])

  // No frontend cleanup loop: the server-side `list_tabs` handler in
  // src/routes/me.rs filters out tabs whose underlying item is gone,
  // and explicit deletes call `removeTabsForItem` directly. Closing on
  // "not in the sessions list" was overzealous — worker sessions
  // (`is_worker=true`) are intentionally not in `GET /api/sessions`, so
  // their tabs were disappearing the moment the page loaded.

  // Always render the strip — even with zero tabs — so the trailing `+`
  // button stays reachable as the user's entry point to creating a new
  // session.
  return (
    <div className="tabbar" role="tablist" aria-label="Open tabs">
      {tabs.map((t) => {
        const isActive =
          (t.itemType === 'session' && view === 'sessions' && activeSessionId === t.itemId) ||
          (t.itemType === 'project' && view === 'projects' && activeProjectId === t.itemId)
        // Prefer the live name from the sessions/projects stores so
        // an in-page rename reflects on the tab immediately. Fall back
        // to the denormalized `t.name` (the server's tab response
        // carries it) — that's what lets a worker session tab render
        // a real label even though `is_worker=true` excludes the
        // session from the plain sessions list.
        // `||` (not `??`) is intentional: openTab's optimistic insert
        // stores `name: ''` for the brief window between local insert
        // and the upsert response landing, and the empty string must
        // fall through to the placeholder rather than render as a
        // label-less chip.
        const live =
          t.itemType === 'session' ? sessionMap.get(t.itemId)?.name : projectMap.get(t.itemId)?.name
        const label = live || t.name || (t.itemType === 'session' ? 'Session' : 'Project')
        const isRunning = t.itemType === 'session' && processing.has(t.itemId)
        const isUnread = t.itemType === 'session' && !isActive && unreadSessions.has(t.itemId)
        return (
          <OpenedTab
            key={`${t.itemType}:${t.itemId}`}
            type={t.itemType}
            id={t.itemId}
            label={label}
            active={isActive}
            running={isRunning}
            unread={isUnread}
            isWorker={t.isWorker}
            onClick={() => onOpenItem(t.itemType, t.itemId)}
            onClose={() => closeTab(t.itemType, t.itemId)}
            onRename={() => onRenameItem(t.itemType, t.itemId)}
            onClear={() => onClearItem(t.itemType, t.itemId)}
            onDelete={() => onDeleteItem(t.itemType, t.itemId)}
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

function OpenedTab({
  type,
  id,
  label,
  active,
  running,
  unread,
  isWorker,
  onClick,
  onClose,
  onRename,
  onClear,
  onDelete,
}: {
  type: TabType
  id: string
  label: string
  active: boolean
  running: boolean
  unread: boolean
  isWorker: boolean
  onClick: () => void
  onClose: () => void
  onRename: () => void
  onClear: () => void
  onDelete: () => void
}) {
  const { triggerProps, menu, consumeLongPressClick } = useContextMenu((): ContextMenuItem[] => [
    { label: 'Close tab', onSelect: onClose },
    { label: 'Rename', onSelect: onRename },
    // Clear messages is session-specific — there's no equivalent for
    // projects, so hide rather than disable to avoid menu noise.
    { label: 'Clear messages', onSelect: onClear, hidden: type !== 'session' },
    {
      label: type === 'session' ? 'Delete session' : 'Delete project',
      onSelect: onDelete,
      danger: true,
      // Worker sessions are owned by their card — the backend refuses
      // DELETE /api/sessions/:id for them, so hide rather than render a
      // button that always 409s.
      hidden: type === 'session' && isWorker,
    },
  ])

  return (
    <div className="tab-wrap" data-tab-id={`${type}:${id}`}>
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
        {type === 'project' && (
          <span className={`tab-icon tab-icon-${type}`} aria-hidden="true">
            ◧
          </span>
        )}
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
