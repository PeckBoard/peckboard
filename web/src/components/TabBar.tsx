import { useEffect, useMemo, useRef, useState } from 'react'
import { useSessionsStore } from '../store/sessions'
import { useProjectsStore } from '../store/projects'
import { useTabsStore, type TabType } from '../store/tabs'

interface TabBarProps {
  view: 'sessions' | 'projects' | 'folders' | 'settings' | 'reports' | 'git' | 'users'
  activeSessionId: string | null
  activeProjectId: string | null
  onOpenItem: (type: TabType, id: string) => void
  onDeleteItem: (type: TabType, id: string) => void
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
 *     also right-click → context menu with Close.
 *   Mobile:  long-press → context menu with Close. The X is hidden
 *     under the 768px breakpoint to keep tab chips compact.
 */
export default function TabBar({
  view,
  activeSessionId,
  activeProjectId,
  onOpenItem,
  onDeleteItem,
}: TabBarProps) {
  const tabs = useTabsStore((s) => s.tabs)
  const closeTab = useTabsStore((s) => s.closeTab)
  const sessions = useSessionsStore((s) => s.sessions)
  const sessionsLoaded = useSessionsStore((s) => s.sessionsLoaded)
  const projects = useProjectsStore((s) => s.projects)
  const projectsLoaded = useProjectsStore((s) => s.projectsLoaded)
  const unreadSessions = useSessionsStore((s) => s.unreadSessions)
  const processing = useSessionsStore((s) => s.processing)

  const sessionMap = useMemo(() => new Map(sessions.map((s) => [s.id, s])), [sessions])
  const projectMap = useMemo(() => new Map(projects.map((p) => [p.id, p])), [projects])

  // Drop any tab whose underlying session/project no longer exists.
  // Without this the strip renders a placeholder chip labelled
  // "Session" / "Project" — that's the phantom-tab bug. We only
  // filter once the corresponding store has loaded, otherwise the
  // brief window before the first fetch arrives looks identical to
  // "everything was deleted" and we'd nuke every real tab.
  const visibleTabs = useMemo(
    () =>
      tabs.filter((t) => {
        if (t.itemType === 'session') {
          return !sessionsLoaded || sessionMap.has(t.itemId)
        }
        return !projectsLoaded || projectMap.has(t.itemId)
      }),
    [tabs, sessionsLoaded, projectsLoaded, sessionMap, projectMap],
  )

  // Once both stores have loaded, fire-and-forget close any tabs
  // pointing at vanished items so the cleanup syncs across devices
  // (closeTab DELETEs the row on the server). Run in an effect rather
  // than during render to avoid setState-during-render warnings.
  useEffect(() => {
    if (!sessionsLoaded || !projectsLoaded) return
    for (const t of tabs) {
      const exists = t.itemType === 'session' ? sessionMap.has(t.itemId) : projectMap.has(t.itemId)
      if (!exists) closeTab(t.itemType, t.itemId)
    }
  }, [tabs, sessionsLoaded, projectsLoaded, sessionMap, projectMap, closeTab])

  if (visibleTabs.length === 0) return null

  return (
    <div className="tabbar" role="tablist" aria-label="Open tabs">
      {visibleTabs.map((t) => {
        const isActive =
          (t.itemType === 'session' && view === 'sessions' && activeSessionId === t.itemId) ||
          (t.itemType === 'project' && view === 'projects' && activeProjectId === t.itemId)
        // After the filter above, the lookup may still miss only
        // during the pre-load window — fall back to a generic label
        // there. Once stores are loaded, every visible tab is known
        // to map to a real item.
        const label =
          t.itemType === 'session'
            ? (sessionMap.get(t.itemId)?.name ?? 'Session')
            : (projectMap.get(t.itemId)?.name ?? 'Project')
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
            onClick={() => onOpenItem(t.itemType, t.itemId)}
            onClose={() => closeTab(t.itemType, t.itemId)}
            onDelete={() => onDeleteItem(t.itemType, t.itemId)}
          />
        )
      })}
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
  onClick,
  onClose,
  onDelete,
}: {
  type: TabType
  id: string
  label: string
  active: boolean
  running: boolean
  unread: boolean
  onClick: () => void
  onClose: () => void
  onDelete: () => void
}) {
  const [menuOpen, setMenuOpen] = useState(false)
  const longPressTimer = useRef<number | undefined>(undefined)
  const longPressFired = useRef(false)

  // Close menu when clicking elsewhere.
  useEffect(() => {
    if (!menuOpen) return
    const onDocClick = () => setMenuOpen(false)
    document.addEventListener('click', onDocClick)
    return () => document.removeEventListener('click', onDocClick)
  }, [menuOpen])

  const startLongPress = () => {
    longPressFired.current = false
    longPressTimer.current = window.setTimeout(() => {
      longPressFired.current = true
      setMenuOpen(true)
    }, 450)
  }
  const cancelLongPress = () => {
    if (longPressTimer.current !== undefined) {
      window.clearTimeout(longPressTimer.current)
      longPressTimer.current = undefined
    }
  }

  return (
    <div className="tab-wrap" data-tab-id={`${type}:${id}`}>
      <button
        role="tab"
        aria-selected={active}
        className={`tab tab-opened ${active ? 'tab-active' : ''}`}
        onClick={(e) => {
          // If a long-press just fired, suppress the click that follows.
          if (longPressFired.current) {
            e.preventDefault()
            e.stopPropagation()
            longPressFired.current = false
            return
          }
          onClick()
        }}
        onContextMenu={(e) => {
          // Right-click on desktop opens the same menu as long-press on touch.
          e.preventDefault()
          setMenuOpen(true)
        }}
        onTouchStart={startLongPress}
        onTouchEnd={cancelLongPress}
        onTouchCancel={cancelLongPress}
        onTouchMove={cancelLongPress}
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
      {menuOpen && (
        <div className="tab-menu" role="menu">
          <button
            role="menuitem"
            onClick={(e) => {
              e.stopPropagation()
              setMenuOpen(false)
              onClose()
            }}
          >
            Close tab
          </button>
          <button
            role="menuitem"
            className="tab-menu-danger"
            onClick={(e) => {
              e.stopPropagation()
              setMenuOpen(false)
              onDelete()
            }}
          >
            {type === 'session' ? 'Delete session' : 'Delete project'}
          </button>
        </div>
      )}
    </div>
  )
}
