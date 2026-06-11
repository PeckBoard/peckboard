import { useEffect, useState, useMemo, useCallback, useRef } from 'react'
import { useAuthStore, authedFetch } from './store/auth'
import type { Announcement, PmDecisionsChangedEvent } from './types/api'
import { useUiStore } from './store/ui'
import { useWsStore } from './store/ws'
import { useSessionsStore } from './store/sessions'
import { useProjectsStore } from './store/projects'
import { usePmStore } from './store/pmStore'
import { useFoldersStore } from './store/folders'
import LoginModal from './components/LoginModal'
import ChatView from './components/ChatView'
import SessionTodosView from './components/SessionTodosView'
import List from './components/List'
import ListViewHeader from './components/ListViewHeader'
import ProjectList from './components/ProjectList'
import KanbanBoard from './components/KanbanBoard'
import ProjectTodosView from './components/ProjectTodosView'
import SettingsModal from './components/SettingsModal'
import { applyThemeColor } from './util/themeColor'
import PluginsModal from './components/PluginsModal'
import NewSessionModal from './components/NewSessionModal'
import NewProjectModal from './components/NewProjectModal'
import FoldersPage from './components/ManageFoldersModal'
import ConfirmDialog from './components/ConfirmDialog'
import ReportBrowser from './components/ReportBrowser'
import ReportView from './components/ReportView'
import ExpertsView from './components/ExpertsView'
import PmExpertView from './components/PmExpertView'
import RepeatingTasksView from './components/RepeatingTasksView'
import UsageDashboard from './components/UsageDashboard'
import UserManagement from './components/UserManagement'
import ChangePasswordModal from './components/ChangePasswordModal'
import TabBar from './components/TabBar'
import {
  parseReportTabId,
  reportTabId,
  tabIcons,
  type TabKindHandler,
  type TabKindRegistry,
} from './components/tabKinds'
import { useRepeatingTasksStore } from './store/repeatingTasks'
import ErrorBoundary from './components/ErrorBoundary'
import ConnectionBanner from './components/ConnectionBanner'
import { startTabsAutoSync, useTabsStore, type TabType } from './store/tabs'
import './App.css'

type View =
  | 'sessions'
  | 'repeatingTasks'
  | 'projects'
  | 'experts'
  | 'usage'
  | 'folders'
  | 'reports'
  | 'users'

/** Modals reachable from the user-icon dropdown. The URL maps a couple of
 *  paths (`/settings`, `/plugins`) to opening one of these on mount so
 *  bookmarks and the existing e2e routes still land somewhere useful. */
type DropdownModal = 'settings' | 'plugins' | null

/** Sub-view for an active session or project — 'chat' (the default
 *  ChatView / KanbanBoard) or 'todos' (the dedicated *TodosView reachable at
 *  /{sessions,projects}/{id}/todos). */
type SessionSub = 'chat' | 'todos'

/** Parse the current URL pathname into a view, optional active ID, an
 *  optional sub-view (only meaningful when `view` is 'sessions' or
 *  'projects'), and an optional dropdown-modal hint for the few routes
 *  that map straight to a modal (`/settings`, `/plugins`).
 *
 *  For the reports view, `activeId` is the encoded `<folder>/<file>`
 *  pair the user is reading (the same id used as the report tab's
 *  `item_id`). It's `null` on `/reports` (browser/index). */
function parseRoute(): {
  view: View
  activeId: string | null
  sub: SessionSub
  modal: DropdownModal
} {
  const path = window.location.pathname
  const segments = path.split('/').filter(Boolean)
  const first = segments[0] || 'sessions'
  const id = segments[1] || null
  const third = segments[2] || null

  switch (first) {
    case 'sessions':
      return {
        view: 'sessions',
        activeId: id,
        sub: third === 'todos' ? 'todos' : 'chat',
        modal: null,
      }
    case 'projects':
      return {
        view: 'projects',
        activeId: id,
        sub: third === 'todos' ? 'todos' : 'chat',
        modal: null,
      }
    case 'experts':
      return { view: 'experts', activeId: id, sub: 'chat', modal: null }
    case 'usage':
      return { view: 'usage', activeId: null, sub: 'chat', modal: null }
    case 'repeating-tasks':
      return { view: 'repeatingTasks', activeId: id, sub: 'chat', modal: null }
    case 'folders':
      return { view: 'folders', activeId: null, sub: 'chat', modal: null }
    case 'settings':
      return { view: 'sessions', activeId: null, sub: 'chat', modal: 'settings' }
    case 'plugins':
      return { view: 'sessions', activeId: null, sub: 'chat', modal: 'plugins' }
    case 'reports': {
      // `/reports` — index; `/reports/<folder>/<file>` — single report
      // viewer. We compose the same `<folder>/<file>` id the tab strip
      // uses so both surfaces share one identifier.
      const folder = segments[1]
      const file = segments[2]
      const activeId = folder && file ? `${folder}/${file}` : null
      return { view: 'reports', activeId, sub: 'chat', modal: null }
    }
    case 'users':
      return { view: 'users', activeId: null, sub: 'chat', modal: null }
    default:
      return { view: 'sessions', activeId: null, sub: 'chat', modal: null }
  }
}

/** Build a URL path for a given view, optional ID, and optional sub-view. */
function buildPath(view: View, activeId?: string | null, sub?: SessionSub): string {
  if ((view === 'sessions' || view === 'projects') && activeId && sub === 'todos') {
    return `/${view}/${activeId}/todos`
  }
  if (view === 'repeatingTasks') {
    return activeId ? `/repeating-tasks/${activeId}` : '/repeating-tasks'
  }
  if (view === 'reports') {
    // activeId is the `<folder>/<file>` pair; do NOT encode the slash
    // (it's the path separator). Each segment is already restricted to
    // the safe charset by the reports route, so passing through is OK.
    return activeId ? `/reports/${activeId}` : '/reports'
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
  const sessionsNextCursor = useSessionsStore((s) => s.sessionsNextCursor)
  const sessionsLoadingMore = useSessionsStore((s) => s.sessionsLoadingMore)
  const fetchMoreSessions = useSessionsStore((s) => s.fetchMoreSessions)
  const activeSessionId = useSessionsStore((s) => s.activeSessionId)
  const fetchSessions = useSessionsStore((s) => s.fetchSessions)
  const setActiveSession = useSessionsStore((s) => s.setActiveSession)
  const deleteSession = useSessionsStore((s) => s.deleteSession)
  const renameSession = useSessionsStore((s) => s.renameSession)
  const clearSession = useSessionsStore((s) => s.clearSession)
  const terminateAgent = useSessionsStore((s) => s.terminateAgent)
  const fetchEvents = useSessionsStore((s) => s.fetchEvents)
  const processing = useSessionsStore((s) => s.processing)
  const unreadSessions = useSessionsStore((s) => s.unreadSessions)
  const markSessionRead = useSessionsStore((s) => s.markSessionRead)
  const experts = useSessionsStore((s) => s.experts)
  const expertsLoaded = useSessionsStore((s) => s.expertsLoaded)
  const fetchExperts = useSessionsStore((s) => s.fetchExperts)
  const projects = useProjectsStore((s) => s.projects)
  const projectsLoaded = useProjectsStore((s) => s.projectsLoaded)
  const activeProjectId = useProjectsStore((s) => s.activeProjectId)
  const deleteProject = useProjectsStore((s) => s.deleteProject)
  const updateProject = useProjectsStore((s) => s.updateProject)
  const fetchProjects = useProjectsStore((s) => s.fetchProjects)
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)
  const pmPendingTotal = usePmStore((s) =>
    Object.values(s.pendingCountByProject).reduce((sum, n) => sum + n, 0),
  )

  const folderMap = useMemo(() => {
    const m = new Map<string, string>()
    for (const f of folders) m.set(f.id, f.name)
    return m
  }, [folders])

  // Lookup tables used by the tab strip to resolve a tab's live name
  // (a rename mid-session shows up on the chip instantly, without
  // waiting on the next /api/me/tabs refetch). Kept up here so the
  // TabBar's registry handlers can close over them without paying
  // for a fresh `new Map(...)` per row.
  const sessionMap = useMemo(() => new Map(sessions.map((s) => [s.id, s])), [sessions])
  const projectMap = useMemo(() => new Map(projects.map((p) => [p.id, p])), [projects])

  // Defense-in-depth: experts must never appear in the chat session
  // list. The API (GET /api/sessions) already excludes them, but filter
  // again client-side so a backend regression can't leak them here.
  const chatSessions = useMemo(() => sessions.filter((s) => !s.is_expert), [sessions])

  const setActiveProject = useProjectsStore((s) => s.setActiveProject)

  // Parse initial route
  const initialRoute = useMemo(() => parseRoute(), [])
  const [view, setViewRaw] = useState<View>(initialRoute.view)
  const [sessionSub, setSessionSub] = useState<SessionSub>(initialRoute.sub)
  const [activeRepeatingTaskId, setActiveRepeatingTaskId] = useState<string | null>(
    initialRoute.view === 'repeatingTasks' ? initialRoute.activeId : null,
  )
  // Active report — encoded `<folder>/<file>` pair when the route is
  // `/reports/:folder/:file`, null on the `/reports` index. Kept as a
  // single string (not split into two `useState`s) so the registry can
  // pass it straight to the tab strip as `item_id` and so the storage
  // shape matches what the backend persists in `user_tabs.item_id`.
  const initialReportId =
    initialRoute.view === 'reports' && initialRoute.activeId ? initialRoute.activeId : null
  const [activeReportId, setActiveReportId] = useState<string | null>(initialReportId)
  const repeatingTasks = useRepeatingTasksStore((s) => s.tasks)
  // The expert whose transcript is open (route `/experts/:id`). Tracked
  // locally rather than in a store: experts are deliberately kept out of the
  // session list / tab system, so this never feeds the MRU tab logic.
  const [activeExpertId, setActiveExpertId] = useState<string | null>(
    initialRoute.view === 'experts' ? initialRoute.activeId : null,
  )
  // Resolve the open expert so the detail surface can branch on kind:
  // the PM expert renders a Q&A form, every other kind keeps ChatView.
  const activeExpert = useMemo(
    () => (activeExpertId ? (experts.find((e) => e.id === activeExpertId) ?? null) : null),
    [experts, activeExpertId],
  )
  const [showNewSession, setShowNewSession] = useState(false)
  const [showNewProject, setShowNewProject] = useState(false)
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null)
  const [selectedSessions, setSelectedSessions] = useState<Set<string>>(() => new Set())
  const [confirmDeleteProjectId, setConfirmDeleteProjectId] = useState<string | null>(null)
  const [confirmClearSessionId, setConfirmClearSessionId] = useState<string | null>(null)
  const [confirmTerminateSessionId, setConfirmTerminateSessionId] = useState<string | null>(null)
  const [confirmDeleteRepeatingTaskId, setConfirmDeleteRepeatingTaskId] = useState<string | null>(
    null,
  )
  const [announcement, setAnnouncement] = useState<Announcement | null>(null)
  const [userMenuOpen, setUserMenuOpen] = useState(false)
  const [showChangePassword, setShowChangePassword] = useState(false)
  const [dropdownModal, setDropdownModal] = useState<DropdownModal>(initialRoute.modal)
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

  // Open one of the dropdown-modal targets (Settings / Plugins) and reflect
  // it in the URL so the modal is bookmarkable and survives a reload. The
  // close handler simply navigates back, which pops the entry off history
  // and clears `dropdownModal` via popstate.
  const openDropdownModal = useCallback((target: Exclude<DropdownModal, null>) => {
    setDropdownModal(target)
    const path = `/${target}`
    if (window.location.pathname !== path) {
      history.pushState(null, '', path)
    }
  }, [])

  const closeDropdownModal = useCallback(() => {
    // Just clear modal state. The URL-sync effect notices `dropdownModal`
    // went null and pushes the underlying view's path (typically `/`),
    // which moves us off `/settings` or `/plugins` so a Back press
    // re-enters the modal — natural Back/Forward UX.
    setDropdownModal(null)
  }, [])

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
      setDropdownModal(route.modal)
      if (route.view === 'sessions') {
        setActiveSession(route.activeId)
      } else if (route.view === 'projects') {
        setActiveProject(route.activeId)
      } else if (route.view === 'repeatingTasks') {
        setActiveRepeatingTaskId(route.activeId)
      } else if (route.view === 'experts') {
        setActiveExpertId(route.activeId)
      } else if (route.view === 'reports') {
        setActiveReportId(route.activeId)
      }
    }
    window.addEventListener('popstate', onPopState)
    return () => window.removeEventListener('popstate', onPopState)
  }, [setActiveSession, setActiveProject])

  // When activeSessionId changes, update URL — unless we're sitting on a
  // dropdown-modal route (`/settings` or `/plugins`). Those routes map to
  // `view: 'sessions'` but must keep their pathname so a reload still
  // lands on the same modal; without this guard the sync rewrites
  // `/plugins` back to `/` on first render and the modal vanishes.
  useEffect(() => {
    if (dropdownModal) return
    if (view === 'sessions') {
      const path = buildPath('sessions', activeSessionId, activeSessionId ? sessionSub : 'chat')
      if (window.location.pathname !== path) {
        history.pushState(null, '', path)
      }
    }
  }, [view, activeSessionId, sessionSub, dropdownModal])

  // When activeProjectId changes, update URL.
  useEffect(() => {
    if (dropdownModal) return
    if (view === 'projects') {
      const path = buildPath('projects', activeProjectId, activeProjectId ? sessionSub : 'chat')
      if (window.location.pathname !== path) {
        history.pushState(null, '', path)
      }
    }
  }, [view, activeProjectId, sessionSub, dropdownModal])

  // When the active repeating task changes, update URL.
  useEffect(() => {
    if (dropdownModal) return
    if (view === 'repeatingTasks') {
      const path = buildPath('repeatingTasks', activeRepeatingTaskId)
      if (window.location.pathname !== path) {
        history.pushState(null, '', path)
      }
    }
  }, [view, activeRepeatingTaskId, dropdownModal])

  // When the active report changes, update URL.
  useEffect(() => {
    if (dropdownModal) return
    if (view === 'reports') {
      const path = buildPath('reports', activeReportId)
      if (window.location.pathname !== path) {
        history.pushState(null, '', path)
      }
    }
  }, [view, activeReportId, dropdownModal])

  useEffect(() => {
    const saved = localStorage.getItem('peckboard_theme')
    if (saved === 'dark' || saved === 'light') {
      document.documentElement.setAttribute('data-theme', saved)
      applyThemeColor(saved)
    } else {
      applyThemeColor('auto')
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

    // Only gate the shrink on Stage-Manager-capable widths (iPad-sized).
    // On phones, document.hasFocus() can transiently return false while
    // the keyboard animates open — which then unshrinks the layout and
    // strands the input bar behind the keyboard. Phones don't have
    // Stage Manager, so the isEditableFocused() check below is enough.
    const isTablet = () => window.matchMedia('(min-width: 768px)').matches

    const update = () => {
      let height = `${window.innerHeight}px`
      const focusOwned = isTablet()
        ? document.hasFocus() && isEditableFocused()
        : isEditableFocused()
      if (vv && focusOwned) {
        const delta = window.innerHeight - vv.height
        // Threshold filters out non-keyboard chrome shifts (e.g. iOS
        // URL bar) which we want `100dvh` semantics for, not a hard
        // pixel pin. 50px is below the smallest realistic soft keyboard
        // (typical iOS/Android keyboards are 240px+) but above the
        // ~30-40px URL-bar collapse on iOS Safari.
        if (delta > 50) height = `${vv.height}px`
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
      // Experts are needed at startup (not just when ExpertsView mounts)
      // so a deep link to /experts/:id can branch on expert_kind — the
      // PM expert opens a Q&A form, not the chat transcript.
      fetchExperts()
      useTabsStore.getState().fetchTabs()
      const stopTabsSync = startTabsAutoSync()
      // PM decision-log live updates: ws.ts re-dispatches the
      // `pm-decisions-changed` broadcast as a window event; apply it to
      // the PM store here so pending counts stay live even when no PM
      // component is mounted.
      const onPmChange = (e: globalThis.Event) => {
        const detail = (e as CustomEvent).detail as { data?: PmDecisionsChangedEvent } | undefined
        const data = detail?.data
        if (data?.projectId) {
          usePmStore.getState().applyPmChange(data.projectId, data.pending_count)
        }
      }
      window.addEventListener('peckboard:pm-decisions-changed', onPmChange)
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
        window.removeEventListener('peckboard:pm-decisions-changed', onPmChange)
      }
    }
  }, [authenticated, connect, disconnect, fetchSessions, fetchProjects, fetchFolders, fetchExperts])

  // Seed pending PM-question counts for the rail badge: fetchPmState also
  // subscribes to the project's `pm-decisions-changed` broadcasts, so one
  // fetch per project keeps the count live thereafter.
  const pmSeededProjects = useRef<Set<string>>(new Set())
  useEffect(() => {
    if (!authenticated) return
    for (const p of projects) {
      if (pmSeededProjects.current.has(p.id)) continue
      pmSeededProjects.current.add(p.id)
      usePmStore
        .getState()
        .fetchPmState(p.id)
        .catch(() => {})
    }
  }, [authenticated, projects])

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

  // Open / promote a tab for an active repeating task. The store
  // refuses to upsert a tab for a non-existent task (server-side 404
  // rolls back the optimistic insert), so even a stale URL can't write
  // a phantom row — we just open the tab and let the GET filter clean
  // up if something raced.
  useEffect(() => {
    if (!authenticated || !activeRepeatingTaskId) return
    useTabsStore.getState().openTab('repeating_task', activeRepeatingTaskId)
  }, [authenticated, activeRepeatingTaskId])

  // Open / promote a tab for an active report. Reports are file-backed
  // and the backend validates existence at upsert time; a stale URL
  // results in a 404 here, which closes the optimistic insert
  // gracefully without leaving a phantom chip.
  useEffect(() => {
    if (!authenticated || !activeReportId) return
    useTabsStore.getState().openTab('report', activeReportId)
  }, [authenticated, activeReportId])

  // When a repeating task is deleted (locally or cross-device), the
  // store cascade pulls the chip from the strip; the RepeatingTasksView
  // itself renders a "Task not found" empty-state on a stale id, so
  // there's no setState-in-effect cleanup to do here. The activeId
  // clears when the user clicks "Back to list".

  if (!initialized) {
    return (
      <div className="loading-screen">
        <img src="/favicon.svg" alt="Peckboard" width="64" height="64" />
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

  const toggleSessionSelected = (id: string) => {
    setSelectedSessions((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
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

  const confirmTerminateSession = async () => {
    if (!confirmTerminateSessionId) return
    const id = confirmTerminateSessionId
    setConfirmTerminateSessionId(null)
    try {
      await terminateAgent(id)
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
    } else if (type === 'project') {
      const current = projects.find((p) => p.id === id)?.name ?? ''
      const next = window.prompt('Rename project:', current)
      if (next && next !== current) {
        try {
          await updateProject(id, { name: next })
        } catch {
          /* ignore */
        }
      }
    } else if (type === 'repeating_task') {
      const current = repeatingTasks.find((t) => t.id === id)?.name ?? ''
      const next = window.prompt('Rename task:', current)
      if (next && next !== current) {
        try {
          await useRepeatingTasksStore.getState().updateTask(id, { name: next })
        } catch {
          /* ignore */
        }
      }
    }
    // Reports are file-backed and named at write time; no rename path.
  }

  // Assemble the per-kind glue the TabBar uses. Adding a new tab kind
  // = adding a new entry here; the TabBar is purely presentational and
  // doesn't know what a "session" or "report" is. The registry shape
  // (`Record<TabType, TabKindHandler>`) means the compiler refuses to
  // build if a new kind is added to `TabType` but missed here.
  //
  // Not memoized: the contents close over store snapshots that change
  // every render anyway (sessionMap, processing, unreadSessions, …),
  // so re-building the registry each render is cheaper than tracking
  // every dep. The TabBar takes the registry as a plain prop and the
  // OpenedTab rows aren't memoized either.
  const sessionKind: TabKindHandler = {
    isActive: (tab) => view === 'sessions' && activeSessionId === tab.itemId,
    getLiveName: (tab) => sessionMap.get(tab.itemId)?.name ?? null,
    getBadges: (tab, active) => ({
      running: processing.has(tab.itemId),
      unread: !active && unreadSessions.has(tab.itemId),
    }),
    getIcon: () => null,
    onActivate: (tab) => {
      setActiveSession(tab.itemId)
      navigate('sessions', tab.itemId)
    },
    getMenuItems: (tab) => [
      { label: 'Rename', onSelect: () => handleRenameItem('session', tab.itemId) },
      { label: 'Clear session', onSelect: () => setConfirmClearSessionId(tab.itemId) },
      { label: 'Terminate agent', onSelect: () => setConfirmTerminateSessionId(tab.itemId) },
      // Worker sessions are owned by their card — the backend refuses
      // DELETE /api/sessions/:id for them. Hide rather than render a
      // button that always 409s.
      {
        label: 'Delete session',
        danger: true,
        onSelect: () => setConfirmDeleteId(tab.itemId),
        hidden: tab.isWorker,
      },
    ],
  }
  const projectKind: TabKindHandler = {
    isActive: (tab) => view === 'projects' && activeProjectId === tab.itemId,
    getLiveName: (tab) => projectMap.get(tab.itemId)?.name ?? null,
    getBadges: () => ({ running: false, unread: false }),
    getIcon: () => tabIcons.project,
    onActivate: (tab) => {
      setActiveProject(tab.itemId)
      navigate('projects', tab.itemId)
    },
    getMenuItems: (tab) => [
      { label: 'Rename', onSelect: () => handleRenameItem('project', tab.itemId) },
      {
        label: 'Delete project',
        danger: true,
        onSelect: () => setConfirmDeleteProjectId(tab.itemId),
      },
    ],
  }
  const repeatingTaskKind: TabKindHandler = {
    isActive: (tab) => view === 'repeatingTasks' && activeRepeatingTaskId === tab.itemId,
    getLiveName: (tab) => repeatingTasks.find((t) => t.id === tab.itemId)?.name ?? null,
    getBadges: () => ({ running: false, unread: false }),
    getIcon: () => tabIcons.repeating_task,
    onActivate: (tab) => {
      setActiveRepeatingTaskId(tab.itemId)
      navigate('repeatingTasks', tab.itemId)
    },
    getMenuItems: (tab) => [
      { label: 'Rename', onSelect: () => handleRenameItem('repeating_task', tab.itemId) },
      {
        label: 'Delete task',
        danger: true,
        onSelect: () => setConfirmDeleteRepeatingTaskId(tab.itemId),
      },
    ],
  }
  const reportKind: TabKindHandler = {
    isActive: (tab) => view === 'reports' && activeReportId === tab.itemId,
    getLiveName: () => null, // reports never rename — t.name from the server is authoritative.
    getBadges: () => ({ running: false, unread: false }),
    getIcon: () => tabIcons.report,
    onActivate: (tab) => {
      setActiveReportId(tab.itemId)
      navigate('reports', tab.itemId)
    },
    // Reports have no delete endpoint and no rename, so the kind-
    // specific menu is empty. The TabBar still layers in "Close tab"
    // at the top, which is enough for the strip.
    getMenuItems: () => [],
  }
  const tabKindRegistry: TabKindRegistry = {
    session: sessionKind,
    project: projectKind,
    repeating_task: repeatingTaskKind,
    report: reportKind,
  }

  const confirmDeleteRepeatingTask = async () => {
    if (!confirmDeleteRepeatingTaskId) return
    const id = confirmDeleteRepeatingTaskId
    setConfirmDeleteRepeatingTaskId(null)
    try {
      await useRepeatingTasksStore.getState().deleteTask(id)
      if (activeRepeatingTaskId === id) setActiveRepeatingTaskId(null)
    } catch {
      /* ignore */
    }
  }

  return (
    <div className="shell">
      {/* Navigation Rail. Sessions / Projects live here so the top tab
          strip can use all of its horizontal space for opened tabs —
          critical on mobile, where the rail becomes a bottom toolbar. */}
      <nav className="rail">
        <div className="rail-top">
          <div className="rail-brand">
            <img src="/favicon.svg" alt="Peckboard" width="24" height="24" />
          </div>
          <button
            className={`rail-btn ${view === 'sessions' && !activeSessionId ? 'active' : ''}`}
            onClick={() => {
              setActiveSession(null)
              navigate('sessions', null)
            }}
            title="Sessions"
            aria-label="Sessions"
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
            className={`rail-btn ${view === 'repeatingTasks' ? 'active' : ''}`}
            onClick={() => navigate('repeatingTasks', null)}
            title="Repeating Tasks"
            aria-label="Repeating Tasks"
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
              <polyline points="1 4 1 10 7 10" />
              <polyline points="23 20 23 14 17 14" />
              <path d="M20.49 9A9 9 0 0 0 5.64 5.64L1 10m22 4l-4.64 4.36A9 9 0 0 1 3.51 15" />
            </svg>
          </button>
          <button
            className={`rail-btn ${view === 'projects' && !activeProjectId ? 'active' : ''}`}
            onClick={() => {
              setActiveProject(null)
              navigate('projects', null)
            }}
            title="Projects"
            aria-label="Projects"
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
          <button
            className={`rail-btn ${view === 'experts' ? 'active' : ''}`}
            onClick={() => {
              setActiveExpertId(null)
              navigate('experts')
            }}
            title="Experts"
            aria-label="Experts"
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
              <path d="M12 2a5 5 0 0 0-5 5c0 1.8 1 3.2 2 4.2.8.8 1 1.3 1 2.3v.5h4v-.5c0-1 .2-1.5 1-2.3 1-1 2-2.4 2-4.2a5 5 0 0 0-5-5z" />
              <line x1="10" y1="19" x2="14" y2="19" />
              <line x1="11" y1="22" x2="13" y2="22" />
            </svg>
            {pmPendingTotal > 0 && (
              <span
                className="unread-dot rail-btn-dot"
                data-testid="pm-expert-waiting-badge"
                aria-label="PM expert waiting for answers"
                title="PM expert waiting for answers"
              />
            )}
          </button>
          <button
            className={`rail-btn ${view === 'reports' ? 'active' : ''}`}
            onClick={() => navigate('reports')}
            title="Reports"
            aria-label="Reports"
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
            className={`rail-btn ${view === 'usage' ? 'active' : ''}`}
            onClick={() => navigate('usage')}
            title="Usage"
            aria-label="Usage"
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
              <path d="M3 3v18h18" />
              <rect x="7" y="11" width="3" height="6" />
              <rect x="13" y="7" width="3" height="10" />
            </svg>
          </button>
          <div className="rail-separator" aria-hidden="true" />
          <button
            className={`rail-btn ${view === 'folders' ? 'active' : ''}`}
            onClick={() => navigate('folders')}
            title="Folders"
            aria-label="Folders"
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
          {user?.role === 'admin' && (
            <button
              className={`rail-btn ${view === 'users' ? 'active' : ''}`}
              onClick={() => navigate('users')}
              title="Users"
              aria-label="Users"
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
            role="status"
            aria-label={connected ? 'Connected' : 'Disconnected'}
          />
          <div className="user-menu" ref={userMenuRef}>
            <button
              className="rail-btn rail-avatar"
              onClick={() => setUserMenuOpen((open) => !open)}
              title={user?.username}
              aria-label="User menu"
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
                    openDropdownModal('settings')
                  }}
                >
                  Settings
                </button>
                <button
                  type="button"
                  role="menuitem"
                  onClick={() => {
                    setUserMenuOpen(false)
                    openDropdownModal('plugins')
                  }}
                >
                  Plugins
                </button>
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
        <TabBar kinds={tabKindRegistry} onNewSession={() => setShowNewSession(true)} />
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
        <ConnectionBanner connected={connected} />
        <ErrorBoundary
          label="view"
          resetKey={`${view}:${activeSessionId}:${activeProjectId}:${activeExpertId}:${sessionSub}`}
        >
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
                <ListViewHeader
                  title="Sessions"
                  actionLabel="+ New session"
                  onAction={() => setShowNewSession(true)}
                />
                <List
                  items={chatSessions}
                  getKey={(s) => s.id}
                  activeId={activeSessionId}
                  onActivate={(s) => setActiveSession(s.id)}
                  selectedIds={selectedSessions}
                  onToggleSelected={(s) => toggleSessionSelected(s.id)}
                  onClearSelection={() => setSelectedSessions(new Set())}
                  bulkActions={[
                    // No destructive actions in the list — delete lives on the
                    // chat-toolbar 3-dot menu and tab right-click menu, where
                    // the user has the session open and can act intentionally.
                    {
                      label: 'Mark as read',
                      onClick: () => {
                        for (const id of Array.from(selectedSessions)) markSessionRead(id)
                        setSelectedSessions(new Set())
                      },
                      hidden: ![...selectedSessions].some((id) => unreadSessions.has(id)),
                    },
                  ]}
                  renderItem={(s) => (
                    <>
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
                    </>
                  )}
                  onScroll={(e) => {
                    if (!sessionsNextCursor || sessionsLoadingMore) return
                    const el = e.currentTarget
                    if (el.scrollHeight - el.scrollTop - el.clientHeight < 200) {
                      void fetchMoreSessions()
                    }
                  }}
                  emptyState={
                    <div className="list-view-empty">
                      <p>No sessions yet</p>
                      <button
                        className="list-view-empty-action"
                        onClick={() => setShowNewSession(true)}
                      >
                        Create your first session
                      </button>
                    </div>
                  }
                  footer={
                    sessionsLoadingMore ? (
                      <div className="list-view-loading-more" data-testid="sessions-loading-more">
                        Loading more sessions…
                      </div>
                    ) : null
                  }
                />
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
          {view === 'experts' &&
            (activeExpertId ? (
              activeExpert?.expert_kind === 'pm' && activeExpert.project_id ? (
                <PmExpertView
                  projectId={activeExpert.project_id}
                  expertName={activeExpert.name}
                  onBack={() => {
                    setActiveExpertId(null)
                    navigate('experts')
                  }}
                />
              ) : expertsLoaded || activeExpert ? (
                <ChatView sessionId={activeExpertId} />
              ) : (
                // Deep link before the experts list resolves: hold off on
                // mounting ChatView so a PM expert never flashes a chat
                // transcript while its kind is still unknown.
                <div className="list-view">
                  <div className="list-view-empty">
                    <p>Loading expert…</p>
                  </div>
                </div>
              )
            ) : (
              <ExpertsView
                onOpenExpert={(id) => {
                  setActiveExpertId(id)
                  navigate('experts', id)
                }}
              />
            ))}
          {view === 'repeatingTasks' && (
            <RepeatingTasksView
              activeTaskId={activeRepeatingTaskId}
              onNavigate={(id) => {
                setActiveRepeatingTaskId(id)
                navigate('repeatingTasks', id)
              }}
              onOpenSession={(id) => {
                setActiveSession(id)
                navigate('sessions', id)
              }}
            />
          )}
          {view === 'usage' && <UsageDashboard />}
          {view === 'folders' && <FoldersPage />}
          {view === 'reports' &&
            (activeReportId ? (
              (() => {
                const parsed = parseReportTabId(activeReportId)
                if (!parsed) {
                  // Malformed id: drop back to the index.
                  setActiveReportId(null)
                  return null
                }
                return (
                  <ReportView
                    folder={parsed.folder}
                    file={parsed.file}
                    onBack={() => {
                      setActiveReportId(null)
                      navigate('reports', null)
                    }}
                    onOpenSession={(id) => {
                      setActiveSession(id)
                      navigate('sessions', id)
                    }}
                  />
                )
              })()
            ) : (
              <ReportBrowser
                onOpenReport={(folder, file) => {
                  const id = reportTabId(folder, file)
                  setActiveReportId(id)
                  navigate('reports', id)
                }}
              />
            ))}
          {view === 'users' && <UserManagement />}
        </ErrorBoundary>
      </main>

      {showNewSession && <NewSessionModal onClose={() => setShowNewSession(false)} />}
      {showNewProject && <NewProjectModal onClose={() => setShowNewProject(false)} />}
      {showChangePassword && (
        <ChangePasswordModal mode={{ kind: 'self' }} onClose={() => setShowChangePassword(false)} />
      )}
      {dropdownModal === 'settings' && <SettingsModal onClose={closeDropdownModal} />}
      {dropdownModal === 'plugins' && <PluginsModal onClose={closeDropdownModal} />}
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
      {confirmTerminateSessionId && (
        <ConfirmDialog
          title="Terminate agent"
          message="Terminate the agent process? Any in-flight turn will be interrupted. The next message will start a fresh process."
          confirmLabel="Terminate"
          cancelLabel="Cancel"
          danger
          onConfirm={confirmTerminateSession}
          onCancel={() => setConfirmTerminateSessionId(null)}
        />
      )}
      {confirmDeleteRepeatingTaskId && (
        <ConfirmDialog
          title="Delete repeating task"
          message="Delete this task? Previously spawned sessions are kept but the schedule will stop."
          confirmLabel="Delete"
          cancelLabel="Cancel"
          danger
          onConfirm={confirmDeleteRepeatingTask}
          onCancel={() => setConfirmDeleteRepeatingTaskId(null)}
        />
      )}
    </div>
  )
}

export default App
