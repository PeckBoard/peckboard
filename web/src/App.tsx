import { useEffect, useState, useMemo } from 'react'
import { useAuthStore } from './store/auth'
import { useUiStore } from './store/ui'
import { useWsStore } from './store/ws'
import { useSessionsStore } from './store/sessions'
import { useProjectsStore } from './store/projects'
import { useFoldersStore } from './store/folders'
import LoginModal from './components/LoginModal'
import RegisterModal from './components/RegisterModal'
import ChatView from './components/ChatView'
import ProjectList from './components/ProjectList'
import KanbanBoard from './components/KanbanBoard'
import SettingsPage from './components/SettingsPage'
import NewSessionModal from './components/NewSessionModal'
import NewProjectModal from './components/NewProjectModal'
import FoldersPage from './components/ManageFoldersModal'
import ConfirmDialog from './components/ConfirmDialog'
import './App.css'

type View = 'sessions' | 'projects' | 'folders' | 'settings'

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
  const needsRegistration = useAuthStore((s) => s.needsRegistration)
  const user = useAuthStore((s) => s.user)
  const checkAuth = useAuthStore((s) => s.checkAuth)
  const logout = useAuthStore((s) => s.logout)
  const connected = useUiStore((s) => s.connected)
  const connect = useWsStore((s) => s.connect)
  const disconnect = useWsStore((s) => s.disconnect)
  const sessions = useSessionsStore((s) => s.sessions)
  const activeSessionId = useSessionsStore((s) => s.activeSessionId)
  const fetchSessions = useSessionsStore((s) => s.fetchSessions)
  const setActiveSession = useSessionsStore((s) => s.setActiveSession)
  const deleteSession = useSessionsStore((s) => s.deleteSession)
  const processing = useSessionsStore((s) => s.processing)
  const unreadSessions = useSessionsStore((s) => s.unreadSessions)
  const activeProjectId = useProjectsStore((s) => s.activeProjectId)
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)

  const folderMap = useMemo(() => {
    const m = new Map<string, string>()
    for (const f of folders) m.set(f.id, f.name)
    return m
  }, [folders])

  const [view, setView] = useState<View>('sessions')
  const [showNewSession, setShowNewSession] = useState(false)
  const [showNewProject, setShowNewProject] = useState(false)
  const [contextSession, setContextSession] = useState<string | null>(null)
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null)

  useEffect(() => {
    const saved = localStorage.getItem('peckboard_theme')
    if (saved === 'dark' || saved === 'light') {
      document.documentElement.setAttribute('data-theme', saved)
    }
  }, [])

  useEffect(() => { checkAuth() }, [checkAuth])

  useEffect(() => {
    if (authenticated) {
      connect()
      fetchSessions()
      fetchFolders()
      return () => { disconnect() }
    }
  }, [authenticated, connect, disconnect, fetchSessions, fetchFolders])

  if (!initialized) {
    return (
      <div className="loading-screen">
        <div className="loading-spinner" />
        <span className="loading-text">Peckboard</span>
      </div>
    )
  }

  if (needsRegistration) return <RegisterModal />
  if (!authenticated) return <LoginModal />

  const handleDeleteSession = (id: string) => {
    setConfirmDeleteId(id)
    setContextSession(null)
  }

  const confirmDelete = async () => {
    if (!confirmDeleteId) return
    try { await deleteSession(confirmDeleteId) } catch { /* ignore */ }
    setConfirmDeleteId(null)
  }

  return (
    <div className="shell">
      {/* Navigation Rail */}
      <nav className="rail">
        <div className="rail-top">
          <div className="rail-brand">P</div>
          <button className={`rail-btn ${view === 'sessions' ? 'active' : ''}`} onClick={() => setView('sessions')} title="Sessions">
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" /></svg>
          </button>
          <button className={`rail-btn ${view === 'projects' ? 'active' : ''}`} onClick={() => setView('projects')} title="Projects">
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><rect x="3" y="3" width="7" height="7" /><rect x="14" y="3" width="7" height="7" /><rect x="3" y="14" width="7" height="7" /><rect x="14" y="14" width="7" height="7" /></svg>
          </button>
          <div className="rail-separator" />
          <button className={`rail-btn ${view === 'folders' ? 'active' : ''}`} onClick={() => setView('folders')} title="Folders">
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" /></svg>
          </button>
          <button className={`rail-btn ${view === 'settings' ? 'active' : ''}`} onClick={() => setView('settings')} title="Settings">
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><circle cx="12" cy="12" r="3" /><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" /></svg>
          </button>
        </div>
        <div className="rail-bottom">
          <div className={`rail-status ${connected ? 'online' : ''}`} title={connected ? 'Connected' : 'Disconnected'} />
          <button className="rail-btn rail-avatar" onClick={logout} title={`${user?.username} — Sign out`}>
            {user?.username?.charAt(0).toUpperCase() || '?'}
          </button>
        </div>
      </nav>

      {/* Sidebar Panel */}
      {(view === 'sessions' || view === 'projects') && (
        <aside className="panel">
          {view === 'sessions' && (
            <>
              <div className="panel-header">
                <h2 className="panel-title">Sessions</h2>
                <button className="panel-action" onClick={() => setShowNewSession(true)} title="New session">+</button>
              </div>
              <div className="panel-list">
                {sessions.map((s) => (
                  <div key={s.id} className={`panel-item-row ${s.id === activeSessionId ? 'active' : ''}`}>
                    <button className="panel-item" onClick={() => { setActiveSession(s.id); setContextSession(null) }}>
                      {processing.has(s.id) && <span className="processing-dot" />}
                      {!processing.has(s.id) && unreadSessions.has(s.id) && <span className="unread-dot" />}
                      <span className="panel-item-name">{s.name}</span>
                      <span className="panel-item-meta">
                        {folderMap.get(s.folder_id) && <span className="panel-item-tag">{folderMap.get(s.folder_id)}</span>}
                        <span className="panel-item-time">{formatRelativeTime(s.last_activity)}</span>
                      </span>
                    </button>
                    <button className="panel-item-menu" onClick={(e) => { e.stopPropagation(); setContextSession(contextSession === s.id ? null : s.id) }}>···</button>
                    {contextSession === s.id && (
                      <div className="panel-item-dropdown">
                        <button onClick={() => handleDeleteSession(s.id)}>Delete session</button>
                      </div>
                    )}
                  </div>
                ))}
                {sessions.length === 0 && (
                  <div className="panel-empty">
                    <p>No sessions yet</p>
                    <button className="panel-empty-action" onClick={() => setShowNewSession(true)}>Create your first session</button>
                  </div>
                )}
              </div>
            </>
          )}
          {view === 'projects' && <ProjectList onNewProject={() => setShowNewProject(true)} />}
        </aside>
      )}

      {/* Main Content */}
      <main className="content">
        {view === 'sessions' && (activeSessionId ? <ChatView sessionId={activeSessionId} /> : (
          <div className="empty-state">
            <div className="empty-icon"><svg width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" /></svg></div>
            <h3 className="empty-title">No session selected</h3>
            <p className="empty-desc">Choose a session from the sidebar or create a new one.</p>
            <button className="empty-action" onClick={() => setShowNewSession(true)}>New Session</button>
          </div>
        ))}
        {view === 'projects' && (activeProjectId ? <KanbanBoard projectId={activeProjectId} /> : (
          <div className="empty-state">
            <div className="empty-icon"><svg width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5"><rect x="3" y="3" width="7" height="7" /><rect x="14" y="3" width="7" height="7" /><rect x="3" y="14" width="7" height="7" /><rect x="14" y="14" width="7" height="7" /></svg></div>
            <h3 className="empty-title">No project selected</h3>
            <p className="empty-desc">Select a project or create a new one.</p>
            <button className="empty-action" onClick={() => setShowNewProject(true)}>New Project</button>
          </div>
        ))}
        {view === 'folders' && <FoldersPage />}
        {view === 'settings' && <SettingsPage />}
      </main>

      {showNewSession && <NewSessionModal onClose={() => setShowNewSession(false)} />}
      {showNewProject && <NewProjectModal onClose={() => setShowNewProject(false)} />}
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
    </div>
  )
}

export default App
