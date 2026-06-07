import { useEffect, useRef, useState } from 'react'
import { useSessionsStore } from '../store/sessions'
import { useProjectsStore } from '../store/projects'
import { useTabsStore, type TabType } from '../store/tabs'

interface TabBarProps {
  view: 'sessions' | 'projects' | 'folders' | 'settings' | 'reports' | 'git' | 'users'
  activeSessionId: string | null
  activeProjectId: string | null
  onOpenItem: (type: TabType, id: string) => void
}

/**
 * Top tab strip showing the user's opened sessions and projects mixed
 * together in MRU order, persisted server-side via `useTabsStore` so
 * the same set shows up on every device. The Sessions / Projects list
 * entries live in the navigation rail — keeping them out of here means
 * the strip can use all of its horizontal space for tabs, which matters
 * on mobile where the rail is the bottom toolbar.
 *
 * Close UX: long-press on touch, right-click on mouse → a small context
 * menu offers Close. The X-on-every-tab pattern is too noisy on mobile
 * given how narrow tab chips need to be.
 */
export default function TabBar({
  view,
  activeSessionId,
  activeProjectId,
  onOpenItem,
}: TabBarProps) {
  const tabs = useTabsStore((s) => s.tabs)
  const closeTab = useTabsStore((s) => s.closeTab)
  const sessions = useSessionsStore((s) => s.sessions)
  const projects = useProjectsStore((s) => s.projects)
  const unreadSessions = useSessionsStore((s) => s.unreadSessions)
  const processing = useSessionsStore((s) => s.processing)

  const sessionMap = new Map(sessions.map((s) => [s.id, s]))
  const projectMap = new Map(projects.map((p) => [p.id, p]))

  if (tabs.length === 0) return null

  return (
    <div className="tabbar" role="tablist" aria-label="Open tabs">
      {tabs.map((t) => {
        const isActive =
          (t.itemType === 'session' && view === 'sessions' && activeSessionId === t.itemId) ||
          (t.itemType === 'project' && view === 'projects' && activeProjectId === t.itemId)
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
}: {
  type: TabType
  id: string
  label: string
  active: boolean
  running: boolean
  unread: boolean
  onClick: () => void
  onClose: () => void
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
        <span className={`tab-icon tab-icon-${type}`} aria-hidden="true">
          {type === 'session' ? '#' : '◧'}
        </span>
        {running ? (
          <span className="tab-dot tab-dot-running" aria-label="running" />
        ) : unread ? (
          <span className="tab-dot tab-dot-unread" aria-label="unread" />
        ) : null}
        <span className="tab-label">{label}</span>
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
        </div>
      )}
    </div>
  )
}
