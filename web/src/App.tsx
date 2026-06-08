import { useEffect, useState, useMemo, useCallback, useRef } from 'react'
import { useAuthStore, authedFetch } from './store/auth'
import type { Announcement } from './types/api'
import { useUiStore } from './store/ui'
import { useWsStore } from './store/ws'
import { useSessionsStore } from './store/sessions'
import { useProjectsStore } from './store/projects'
import { useFoldersStore } from './store/folders'
import LoginModal from './components/LoginModal'
import ChatView from './components/ChatView'
import SessionTodosView from './components/SessionTodosView'
import ProjectList from './components/ProjectList'
import KanbanBoard from './components/KanbanBoard'
import ProjectTodosView from './components/ProjectTodosView'
import SettingsPage from './components/SettingsPage'
import NewSessionModal from './components/NewSessionModal'
import NewProjectModal from './components/NewProjectModal'
import FoldersPage from './components/ManageFoldersModal'
import ConfirmDialog from './components/ConfirmDialog'
import ReportBrowser from './components/ReportBrowser'
import GitView from './components/GitView'
import UserManagement from './components/UserManagement'
import ChangePasswordModal from './components/ChangePasswordModal'
import TabBar from './components/TabBar'
import { startTabsAutoSync, useTabsStore, type TabType } from './store/tabs'
import './App.css'

type View = 'sessions' | 'projects' | 'folders' | 'settings' | 'reports' | 'git' | 'users'

/** Sub-view for an active session or project — 'chat' (the default
 *  ChatView / KanbanBoard) or 'todos' (the dedicated *TodosView reachable at
 *  /{sessions,projects}/{id}/todos). */
type SessionSub = 'chat' | 'todos'

/** Parse the current URL pathname into a view, optional active ID, and an
 *  optional sub-view (only meaningful when `view` is 'sessions' or
 *  'projects'). */
function parseRoute(): { view: View; activeId: string | null; sub: SessionSub } {
  const path = window.location.pathname
  const segments = path.split('/').filter(Boolean)
  const first = segments[0] || 'sessions'
  const id = segments[1] || null
  const third = segments[2] || null

  switch (first) {
    case 'sessions':
      return { view: 'sessions', activeId: id, sub: third === 'todos' ? 'todos' : 'chat' }
    case 'projects':
      return { view: 'projects', activeId: id, sub: third === 'todos' ? 'todos' : 'chat' }
    case 'folders':
      return { view: 'folders', activeId: null, sub: 'chat' }
    case 'settings':
      return { view: 'settings', activeId: null, sub: 'chat' }
    case 'reports':
      return { view: 'reports', activeId: null, sub: 'chat' }
    case 'git':
      return { view: 'git', activeId: null, sub: 'chat' }
    case 'users':
      return { view: 'users', activeId: null, sub: 'chat' }
    default:
      return { view: 'sessions', activeId: null, sub: 'chat' }
  }
}

/** Build a URL path for a given view, optional ID, and optional sub-view. */
function buildPath(view: View, activeId?: string | null, sub?: SessionSub): string {
  if ((view === 'sessions' || view === 'projects') && activeId && sub === 'todos') {
    return `/${view}/${activeId}/todos`
  }
  if (activeId) return `/${view}/${activeId}`
  if (view === 'sessions') return '/'
  return `/${view}`
}

function formatRelativeTime(dateStr: string): string {
  const now = Date.now()
  const then = new Date(dateStr).getTime()
  const diffMs = now - then
  if (diffMs < 0) return 'just now'
  const seconds = Math.floor(diffMs / 1000)
  if (seconds < 60) return 'just now'
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes}m ago`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours}h ago`
  const days = Math.floor(hours / 24)
  if (days === 1) return 'yesterday'
  if (days < 30) return `${days}d ago`
  const months = Math.floor(days / 30)
  return `${months}mo ago`
}

function App() {
  const initialized = useAuthStore((s) => s.initialized)
  const authenticated = useAuthStore((s) => s.authenticated)
  const user = useAuthStore((s) => s.user)
  const checkAuth = useAuthStore((s) => s.checkAuth)
  const logout = useAuthStore((s) => s.logout)
  const connected = useUiStore((s) => s.connected)
  const connect = useWsStore((s) => s.connect)
  const disconnect = useWsStore((s) => s.disconnect)
  const sessions = useSessionsStore((s) => s.sessions)
  const sessionsLoaded = useSessionsStore((s) => s.sessionsLoaded)
  const activeSessionId = useSessionsStore((s) => s.activeSessionId)
  const fetchSessions = useSessionsStore((s) => s.fetchSessions)
  const setActiveSession = useSessionsStore((s) => s.setActiveSession)
  const deleteSession = useSessionsStore((s) => s.deleteSession)
  const renameSession = useSessionsStore((s) => s.renameSession)
  const clearSession = useSessionsStore((s) => s.clearSession)
  const fetchEvents = useSessionsStore((s) => s.fetchEvents)
  const processing = useSessionsStore((s) => s.processing)
  const unreadSessions = useSessionsStore((s) => s.unreadSessions)
  const projects = useProjectsStore((s) => s.projects)
  const projectsLoaded = useProjectsStore((s) => s.projectsLoaded)
  const activeProjectId = useProjectsStore((s) => s.activeProjectId)
  const deleteProject = useProjectsStore((s) => s.deleteProject)
  const updateProject = useProjectsStore((s) => s.updateProject)
  const fetchProjects = useProjectsStore((s) => s.fetchProjects)
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)

  const folderMap = useMemo(() => {
    const m = new Map<string, string>()
    for (const f of folders) m.set(f.id, f.name)
    return m
  }, [folders])

  const setActiveProject = useProjectsStore((s) => s.setActiveProject)

  // Parse initial route
  const initialRoute = useMemo(() => parseRoute(), [])
  const [view, setViewRaw] = useState<View>(initialRoute.view)
  const [sessionSub, setSessionSub] = useState<SessionSub>(initialRoute.sub)
  const [showNewSession, setShowNewSession] = useState(false)
  const [showNewProject, setShowNewProject] = useState(false)
  const [contextSession, setContextSession] = useState<string | null>(null)
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null)
  const [confirmDeleteProjectId, setConfirmDeleteProjectId] = useState<string | null>(null)
  const [confirmClearSessionId, setConfirmClearSessionId] = useState<string | null>(null)
  const [announcement, setAnnouncement] = useState<Announcement | null>(null)
  const [userMenuOpen, setUserMenuOpen] = useState(false)
  const [showChangePassword, setShowChangePassword] = useState(false)
  const userMenuRef = useRef<HTMLDivElement | null>(null)

  useEffect(() => {
    if (!userMenuOpen) return
    const onClick = (e: MouseEvent) => {
      if (userMenuRef.current && !userMenuRef.current.contains(e.target as Node)) {
        setUserMenuOpen(false)
      }
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setUserMenuOpen(false)
    }
    document.addEventListener('mousedown', onClick)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onClick)
      document.removeEventListener('keydown', onKey)
    }
  }, [userMenuOpen])

  // Navigate: update view + push URL
  const navigate = useCallback(
    (newView: View, activeId?: string | null, sub: SessionSub = 'chat') => {
      setViewRaw(newView)
      setSessionSub(sub)
      const path = buildPath(newView, activeId, sub)
      if (window.location.pathname !== path) {
        history.pushState(null, '', path)
      }
    },
    [],
  )

  // Sync active IDs from initial URL once authenticated
  useEffect(() => {
    if (authenticated && initialRoute.activeId) {
      if (initialRoute.view === 'sessions') {
        setActiveSession(initialRoute.activeId)
      } else if (initialRoute.view === 'projects') {
        setActiveProject(initialRoute.activeId)
      }
    }
  }, [authenticated, initialRoute, setActiveSession, setActiveProject])

  // Listen for popstate (back/forward)
  useEffect(() => {
    const onPopState = () => {
      const route = parseRoute()
      setViewRaw(route.view)
      setSessionSub(route.sub)
      if (route.view === 'sessions') {
        setActiveSession(route.activeId)
      } else if (route.view === 'projects') {
        setActiveProject(route.activeId)
      }
    }
    window.addEventListener('popstate', onPopState)
    return () => window.removeEventListener('popstate', onPopState)
  }, [setActiveSession, setActiveProject])

  // When activeSessionId changes, update URL.
  useEffect(() => {
    if (view === 'sessions') {
      const path = buildPath('sessions', activeSessionId, activeSessionId ? sessionSub : 'chat')
      if (window.location.pathname !== path) {
        history.pushState(null, '', path)
      }
    }
  }, [view, activeSessionId, sessionSub])

  // When activeProjectId changes, update URL.
  useEffect(() => {
    if (view === 'projects') {
      const path = buildPath('projects', activeProjectId, activeProjectId ? sessionSub : 'chat')
      if (window.location.pathname !== path) {
        history.pushState(null, '', path)
      }
    }
  }, [view, activeProjectId, sessionSub])

  useEffect(() => {
    const saved = localStorage.getItem('peckboard_theme')
    if (saved === 'dark' || saved === 'light') {
      document.documentElement.setAttribute('data-theme', saved)
    }
  }, [])

  // Track the on-screen keyboard via `visualViewport` and shrink the app
  // to the visible region so the top of the UI doesn't scroll off when
  // an input is focused. Plain `100dvh` doesn't reliably react to the
  // keyboard on iOS Safari.
  //
  // iPad gotcha: in Stage Manager / Split View, the *system* keyboard
  // belongs to another app while our `visualViewport.height` still
  // reports the shrunk size. We must only apply the shrink when *we*
  // own the keyboard — gated on `document.hasFocus()` AND an editable
  // element being focused inside our document. Otherwise leave height
  // at `window.innerHeight`.
  useEffect(() => {
    const root = document.documentElement
    const vv = window.visualViewport

    const isEditableFocused = (): boolean => {
      const el = document.activeElement as HTMLElement | null
      if (!el || el === document.body) return false
      const tag = el.tagName
      if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return true
      return el.isContentEditable
    }

    const update = () => {
      let height = `${window.innerHeight}px`
      if (vv && document.hasFocus() && isEditableFocused()) {
        const delta = window.innerHeight - vv.height
        // Threshold filters out non-keyboard chrome shifts (e.g. iOS
        // URL bar) which we want `100dvh` semantics for, not a hard
        // pixel pin.
        if (delta > 80) height = `${vv.height}px`
      }
      root.style.setProperty('--app-height', height)
      // iOS Safari auto-scrolls the *layout* viewport so a focused
      // input near the bottom stays in view above the keyboard. With
      // our `html { overflow: hidden }` setup that scroll has nowhere
      // to go on the document, but the window itself can still end up
      // at a non-zero scrollY — which on mobile renders as "the page
      // scrolled all the way down" with the top toolbar pushed
      // off-screen. Pin it back to (0,0) whenever an editable element
      // is focused — this catches both the immediate focusin scroll
      // and the second-tick visualViewport scroll that iOS does once
      // the keyboard has fully opened.
      if (isEditableFocused() && (window.scrollX !== 0 || window.scrollY !== 0)) {
        window.scrollTo(0, 0)
      }
    }

    // iOS keeps trying to scroll the window after a focus even after we
    // pin once in `update()` — typically a single follow-up tick. Keep
    // forcing scroll back to (0,0) while an editable element is focused.
    const pinScrollIfFocused = () => {
      if (!isEditableFocused()) return
      if (window.scrollX !== 0 || window.scrollY !== 0) {
        window.scrollTo(0, 0)
      }
    }

    update()
    vv?.addEventListener('resize', update)
    vv?.addEventListener('scroll', update)
    window.addEventListener('resize', update)
    window.addEventListener('focus', update)
    window.addEventListener('blur', update)
    window.addEventListener('scroll', pinScrollIfFocused, { passive: true })
    document.addEventListener('focusin', update)
    document.addEventListener('focusout', update)
    document.addEventListener('visibilitychange', update)

    return () => {
      vv?.removeEventListener('resize', update)
      vv?.removeEventListener('scroll', update)
      window.removeEventListener('resize', update)
      window.removeEventListener('focus', update)
      window.removeEventListener('blur', update)
      window.removeEventListener('scroll', pinScrollIfFocused)
      document.removeEventListener('focusin', update)
      document.removeEventListener('focusout', update)
      document.removeEventListener('visibilitychange', update)
    }
  }, [])

  useEffect(() => {
    checkAuth()
  }, [checkAuth])

  useEffect(() => {
    if (authenticated) {
      connect()
      fetchSessions()
      // Projects list is needed at startup (not just when ProjectList
      // mounts) so the tab strip can validate project tabs against it
      // — otherwise it can't tell an orphan project tab from a
      // not-yet-loaded one and either leaks phantom chips or wrongly
      // closes a real tab.
      fetchProjects()
      fetchFolders()
      useTabsStore.getState().fetchTabs()
      const stopTabsSync = startTabsAutoSync()
      // Fetch announcements
      authedFetch('/api/announcements')
        .then((res) => (res.ok ? res.json() : []))
        .then((data: Announcement[]) => {
          if (Array.isArray(data) && data.length > 0) {
            setAnnouncement(data[0])
          }
        })
        .catch(() => {})
      return () => {
        disconnect()
        stopTabsSync()
      }
    }
  }, [authenticated, connect, disconnect, fetchSessions, fetchProjects, fetchFolders])

  // Open / promote a tab whenever the user activates a session or
  // project — this is what makes "MRU + cross-device sync" Just Work,
  // because every navigation goes through these state changes (whether
  // it came from a tab click, the rail, a URL load, or back/forward).
  //
  // Gate on the store being loaded and the item actually existing.
  // Without that guard, a stale URL like `/sessions/<deleted-id>` (a
  // bookmark, browser history, or another device that just deleted
  // the session) would write a phantom `user_tabs` row that the strip
  // then renders as a chip labelled "Session". If the id is unknown
  // after loading, clear the active id so the URL drops back to the
  // list view.
  useEffect(() => {
    if (!authenticated || !activeSessionId || !sessionsLoaded) return
    if (sessions.some((s) => s.id === activeSessionId)) {
      useTabsStore.getState().openTab('session', activeSessionId)
      return
    }
    // Not in the plain-sessions list. Worker sessions are excluded from
    // GET /api/sessions on purpose (they'd clutter the user's session
    // list), but a card's "View Session" link points straight at the
    // worker's id — so we must verify the session exists rather than
    // assume "not in list = deleted" and drop activeSessionId.
    let cancelled = false
    authedFetch(`/api/sessions/${activeSessionId}`)
      .then((res) => {
        if (cancelled) return
        if (res.ok) {
          useTabsStore.getState().openTab('session', activeSessionId)
        } else {
          setActiveSession(null)
        }
      })
      .catch(() => {
        if (!cancelled) setActiveSession(null)
      })
    return () => {
      cancelled = true
    }
  }, [authenticated, activeSessionId, sessionsLoaded, sessions, setActiveSession])
  useEffect(() => {
    if (!authenticated || !activeProjectId || !projectsLoaded) return
    if (projects.some((p) => p.id === activeProjectId)) {
      useTabsStore.getState().openTab('project', activeProjectId)
    } else {
      setActiveProject(null)
    }
  }, [authenticated, activeProjectId, projectsLoaded, projects, setActiveProject])

  if (!initialized) {
    return (
      <div className="loading-screen">
        <div className="loading-spinner" />
        <span className="loading-text">Peckboard</span>
      </div>
    )
  }

  if (!authenticated) return <LoginModal />

  const dismissAnnouncement = async () => {
    if (!announcement) return
    const id = announcement.id
    setAnnouncement(null)
    try {
      await authedFetch(`/api/announcements/${id}`, { method: 'DELETE' })
    } catch {
      /* ignore */
    }
  }

  const handleDeleteSession = (id: string) => {
    setConfirmDeleteId(id)
    setContextSession(null)
  }

  const confirmDelete = async () => {
    if (!confirmDeleteId) return
    try {
      await deleteSession(confirmDeleteId)
    } catch {
      /* ignore */
    }
    setConfirmDeleteId(null)
  }

  const confirmDeleteProject = async () => {
    if (!confirmDeleteProjectId) return
    try {
      await deleteProject(confirmDeleteProjectId)
    } catch {
      /* ignore */
    }
    setConfirmDeleteProjectId(null)
  }

  const confirmClearSession = async () => {
    if (!confirmClearSessionId) return
    const id = confirmClearSessionId
    setConfirmClearSessionId(null)
    try {
      await clearSession(id)
      // Refetch so the open ChatView (if any) reflects the empty event
      // list immediately — clearSession only wipes our local cache for
      // the cleared session, but the view subscribes by id and won't
      // notice an in-place mutation.
      await fetchEvents(id)
    } catch {
      /* ignore */
    }
  }

  const handleRenameItem = async (type: TabType, id: string) => {
    if (type === 'session') {
      const current = sessions.find((s) => s.id === id)?.name ?? ''
      const next = window.prompt('Rename session:', current)
      if (next && next !== current) {
        try {
          await renameSession(id, next)
        } catch {
          /* ignore */
        }
      }
    } else {
      const current = projects.find((p) => p.id === id)?.name ?? ''
      const next = window.prompt('Rename project:', current)
      if (next && next !== current) {
        try {
          await updateProject(id, { name: next })
        } catch {
          /* ignore */
        }
      }
    }
  }

  return (
    <div className="shell">
      {/* Navigation Rail. Sessions / Projects live here so the top tab
          strip can use all of its horizontal space for opened tabs —
          critical on mobile, where the rail becomes a bottom toolbar. */}
      <nav className="rail">
        <div className="rail-top">
          <div className="rail-brand">P</div>
          <button
            className={`rail-btn ${view === 'sessions' && !activeSessionId ? 'active' : ''}`}
            onClick={() => {
              setActiveSession(null)
              navigate('sessions', null)
            }}
            title="Sessions"
          >
            <svg
              width="18"
              height="18"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" />
            </svg>
          </button>
          <button
            className={`rail-btn ${view === 'projects' && !activeProjectId ? 'active' : ''}`}
            onClick={() => {
              setActiveProject(null)
              navigate('projects', null)
            }}
            title="Projects"
          >
            <svg
              width="18"
              height="18"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <rect x="3" y="3" width="7" height="18" rx="1" />
              <rect x="14" y="3" width="7" height="11" rx="1" />
            </svg>
          </button>
          <div className="rail-separator" aria-hidden="true" />
          <button
            className={`rail-btn ${view === 'folders' ? 'active' : ''}`}
            onClick={() => navigate('folders')}
            title="Folders"
          >
            <svg
              width="18"
              height="18"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
            </svg>
          </button>
          <button
            className={`rail-btn ${view === 'reports' ? 'active' : ''}`}
            onClick={() => navigate('reports')}
            title="Reports"
          >
            <svg
              width="18"
              height="18"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
              <polyline points="14 2 14 8 20 8" />
              <line x1="16" y1="13" x2="8" y2="13" />
              <line x1="16" y1="17" x2="8" y2="17" />
              <polyline points="10 9 9 9 8 9" />
            </svg>
          </button>
          <button
            className={`rail-btn ${view === 'git' ? 'active' : ''}`}
            onClick={() => navigate('git')}
            title="Git"
          >
            <svg
              width="18"
              height="18"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <circle cx="18" cy="18" r="3" />
              <circle cx="6" cy="6" r="3" />
              <path d="M13 6h3a2 2 0 0 1 2 2v7" />
              <line x1="6" y1="9" x2="6" y2="21" />
            </svg>
          </button>
          <button
            className={`rail-btn ${view === 'settings' ? 'active' : ''}`}
            onClick={() => navigate('settings')}
            title="Settings"
          >
            <svg
              width="18"
              height="18"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <circle cx="12" cy="12" r="3" />
              <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
            </svg>
          </button>
          {user?.role === 'admin' && (
            <button
              className={`rail-btn ${view === 'users' ? 'active' : ''}`}
              onClick={() => navigate('users')}
              title="Users"
            >
              <svg
                width="18"
                height="18"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
              >
                <path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" />
                <circle cx="9" cy="7" r="4" />
                <path d="M23 21v-2a4 4 0 0 0-3-3.87" />
                <path d="M16 3.13a4 4 0 0 1 0 7.75" />
              </svg>
            </button>
          )}
        </div>
        <div className="rail-bottom">
          <div
            className={`rail-status ${connected ? 'online' : ''}`}
            title={connected ? 'Connected' : 'Disconnected'}
          />
          <div className="user-menu" ref={userMenuRef}>
            <button
              className="rail-btn rail-avatar"
              onClick={() => setUserMenuOpen((open) => !open)}
              title={user?.username}
              aria-haspopup="menu"
              aria-expanded={userMenuOpen}
            >
              {user?.username?.charAt(0).toUpperCase() || '?'}
            </button>
            {userMenuOpen && (
              <div className="user-menu-dropdown" role="menu">
                <div className="user-menu-header">
                  <div className="user-menu-name">{user?.username}</div>
                  <div className="user-menu-role">{user?.role}</div>
                </div>
                <button
                  type="button"
                  role="menuitem"
                  onClick={() => {
                    setUserMenuOpen(false)
                    setShowChangePassword(true)
                  }}
                >
                  Change password
                </button>
                <button
                  type="button"
                  role="menuitem"
                  className="user-menu-danger"
                  onClick={() => {
                    setUserMenuOpen(false)
                    logout()
                  }}
                >
                  Sign out
                </button>
              </div>
            )}
          </div>
        </div>
      </nav>

      {/* Main Content */}
      <main className="content">
        <TabBar
          view={view}
          activeSessionId={activeSessionId}
          activeProjectId={activeProjectId}
          onOpenItem={(type, id) => {
            if (type === 'session') {
              setActiveSession(id)
              navigate('sessions', id)
            } else {
              setActiveProject(id)
              navigate('projects', id)
            }
          }}
          onRenameItem={handleRenameItem}
          onClearItem={(type, id) => {
            if (type === 'session') setConfirmClearSessionId(id)
          }}
          onDeleteItem={(type, id) => {
            if (type === 'session') setConfirmDeleteId(id)
            else setConfirmDeleteProjectId(id)
          }}
          onNewSession={() => setShowNewSession(true)}
        />
        {announcement && (
          <div className="announcement-banner">
            <div className="announcement-content">
              <strong>{announcement.title}</strong>
              <span>{announcement.message}</span>
            </div>
            <button className="announcement-dismiss" onClick={dismissAnnouncement} type="button">
              Dismiss
            </button>
          </div>
        )}
        {view === 'sessions' &&
          (activeSessionId ? (
            sessionSub === 'todos' ? (
              <SessionTodosView
                sessionId={activeSessionId}
                onBack={() => navigate('sessions', activeSessionId, 'chat')}
              />
            ) : (
              <ChatView
                sessionId={activeSessionId}
                onOpenTodos={() => navigate('sessions', activeSessionId, 'todos')}
              />
            )
          ) : (
            <div className="list-view">
              <div className="list-view-header">
                <h2 className="list-view-title">Sessions</h2>
                <button
                  className="list-view-action"
                  onClick={() => setShowNewSession(true)}
                  title="New session"
                >
                  + New session
                </button>
              </div>
              <div className="list-view-body">
                {sessions.map((s) => (
                  <div
                    key={s.id}
                    className={`list-view-row ${s.id === activeSessionId ? 'active' : ''}`}
                  >
                    <button
                      className="list-view-item"
                      onClick={() => {
                        setActiveSession(s.id)
                        setContextSession(null)
                      }}
                    >
                      {processing.has(s.id) && <span className="processing-dot" />}
                      {!processing.has(s.id) && unreadSessions.has(s.id) && (
                        <span className="unread-dot" />
                      )}
                      <span className="list-view-name">{s.name}</span>
                      <span className="list-view-meta">
                        {folderMap.get(s.folder_id) && (
                          <span className="list-view-tag">{folderMap.get(s.folder_id)}</span>
                        )}
                        <span className="list-view-time">
                          {formatRelativeTime(s.last_activity)}
                        </span>
                      </span>
                    </button>
                    <button
                      className="list-view-menu"
                      onClick={(e) => {
                        e.stopPropagation()
                        setContextSession(contextSession === s.id ? null : s.id)
                      }}
                    >
                      ···
                    </button>
                    {contextSession === s.id && (
                      <div className="list-view-dropdown">
                        <button onClick={() => handleDeleteSession(s.id)}>Delete session</button>
                      </div>
                    )}
                  </div>
                ))}
                {sessions.length === 0 && (
                  <div className="list-view-empty">
                    <p>No sessions yet</p>
                    <button
                      className="list-view-empty-action"
                      onClick={() => setShowNewSession(true)}
                    >
                      Create your first session
                    </button>
                  </div>
                )}
              </div>
            </div>
          ))}
        {view === 'projects' &&
          (activeProjectId ? (
            sessionSub === 'todos' ? (
              <ProjectTodosView
                projectId={activeProjectId}
                onClose={() => navigate('projects', activeProjectId, 'chat')}
              />
            ) : (
              <KanbanBoard
                projectId={activeProjectId}
                onOpenTodos={() => navigate('projects', activeProjectId, 'todos')}
              />
            )
          ) : (
            <div className="list-view">
              <ProjectList onNewProject={() => setShowNewProject(true)} />
            </div>
          ))}
        {view === 'folders' && <FoldersPage />}
        {view === 'settings' && <SettingsPage />}
        {view === 'reports' && <ReportBrowser />}
        {view === 'git' && <GitView />}
        {view === 'users' && <UserManagement />}
      </main>

      {showNewSession && <NewSessionModal onClose={() => setShowNewSession(false)} />}
      {showNewProject && <NewProjectModal onClose={() => setShowNewProject(false)} />}
      {showChangePassword && (
        <ChangePasswordModal mode={{ kind: 'self' }} onClose={() => setShowChangePassword(false)} />
      )}
      {confirmDeleteId && (
        <ConfirmDialog
          title="Delete session"
          message="Delete this session and all its events?"
          confirmLabel="Delete"
          cancelLabel="Cancel"
          danger
          onConfirm={confirmDelete}
          onCancel={() => setConfirmDeleteId(null)}
        />
      )}
      {confirmDeleteProjectId && (
        <ConfirmDialog
          title="Delete project"
          message="Delete this project and all its cards?"
          confirmLabel="Delete"
          cancelLabel="Cancel"
          danger
          onConfirm={confirmDeleteProject}
          onCancel={() => setConfirmDeleteProjectId(null)}
        />
      )}
      {confirmClearSessionId && (
        <ConfirmDialog
          title="Clear session"
          message="Clear all messages in this session? This cannot be undone."
          confirmLabel="Clear"
          cancelLabel="Cancel"
          danger
          onConfirm={confirmClearSession}
          onCancel={() => setConfirmClearSessionId(null)}
        />
      )}
    </div>
  )
}

export default App
