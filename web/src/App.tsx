import { useEffect, useState } from 'react'
import { useAuthStore } from './store/auth'
import { useUiStore } from './store/ui'
import { useWsStore } from './store/ws'
import { useSessionsStore } from './store/sessions'
import { useProjectsStore } from './store/projects'
import LoginModal from './components/LoginModal'
import RegisterModal from './components/RegisterModal'
import ChatView from './components/ChatView'
import ProjectList from './components/ProjectList'
import KanbanBoard from './components/KanbanBoard'
import SettingsPage from './components/SettingsPage'

type Tab = 'sessions' | 'projects' | 'settings'

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
  const activeProjectId = useProjectsStore((s) => s.activeProjectId)

  const [activeTab, setActiveTab] = useState<Tab>('sessions')

  useEffect(() => {
    checkAuth()
  }, [checkAuth])

  useEffect(() => {
    if (authenticated) {
      connect()
      fetchSessions()
      return () => {
        disconnect()
      }
    }
  }, [authenticated, connect, disconnect, fetchSessions])

  if (!initialized) {
    return <div className="loading">Loading...</div>
  }

  if (needsRegistration) {
    return <RegisterModal />
  }

  if (!authenticated) {
    return <LoginModal />
  }

  return (
    <div className="app">
      <header className="app-header">
        <h1>Peckboard</h1>
        <nav className="nav-tabs">
          <button
            className={`nav-tab ${activeTab === 'sessions' ? 'active' : ''}`}
            onClick={() => setActiveTab('sessions')}
          >
            Sessions
          </button>
          <button
            className={`nav-tab ${activeTab === 'projects' ? 'active' : ''}`}
            onClick={() => setActiveTab('projects')}
          >
            Projects
          </button>
          <button
            className={`nav-tab ${activeTab === 'settings' ? 'active' : ''}`}
            onClick={() => setActiveTab('settings')}
          >
            Settings
          </button>
        </nav>
        <div className="header-right">
          <span className={`status ${connected ? 'connected' : 'disconnected'}`}>
            {connected ? 'Connected' : 'Disconnected'}
          </span>
          <span className="user-name">{user?.username}</span>
          <button className="logout-btn" onClick={logout}>
            Logout
          </button>
        </div>
      </header>
      <div className="app-body">
        {activeTab === 'sessions' && (
          <>
            <aside className="sidebar">
              <h2>Sessions</h2>
              <ul className="session-list">
                {sessions.map((session) => (
                  <li key={session.id} className={session.id === activeSessionId ? 'active' : ''}>
                    <button onClick={() => setActiveSession(session.id)}>
                      {session.name}
                    </button>
                  </li>
                ))}
                {sessions.length === 0 && <li className="empty">No sessions</li>}
              </ul>
            </aside>
            <main className="app-main">
              {activeSessionId ? (
                <ChatView sessionId={activeSessionId} />
              ) : (
                <p className="placeholder-text">Select a session to get started.</p>
              )}
            </main>
          </>
        )}
        {activeTab === 'projects' && (
          <>
            <aside className="sidebar">
              <ProjectList />
            </aside>
            <main className="app-main">
              {activeProjectId ? (
                <KanbanBoard projectId={activeProjectId} />
              ) : (
                <p className="placeholder-text">Select a project to view its board.</p>
              )}
            </main>
          </>
        )}
        {activeTab === 'settings' && (
          <main className="app-main">
            <SettingsPage />
          </main>
        )}
      </div>
    </div>
  )
}

export default App
