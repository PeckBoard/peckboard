import { useEffect, useState } from 'react'
import { useAuthStore } from '../store/auth'
import { authedFetch } from '../store/auth'

type Theme = 'light' | 'dark' | 'auto'

const THEME_KEY = 'peckboard_theme'

function getStoredTheme(): Theme {
  const stored = localStorage.getItem(THEME_KEY)
  if (stored === 'light' || stored === 'dark' || stored === 'auto') return stored
  return 'auto'
}

function applyTheme(theme: Theme) {
  const root = document.documentElement
  if (theme === 'auto') {
    root.removeAttribute('data-theme')
  } else {
    root.setAttribute('data-theme', theme)
  }
}

interface ServerConfig {
  port?: number
  host?: string
  data_dir?: string
  [key: string]: unknown
}

export default function SettingsPage() {
  const user = useAuthStore((s) => s.user)
  const [theme, setTheme] = useState<Theme>(getStoredTheme)
  const [serverConfig, setServerConfig] = useState<ServerConfig | null>(null)
  const [configError, setConfigError] = useState('')

  useEffect(() => {
    async function loadConfig() {
      try {
        const res = await authedFetch('/api/health')
        if (res.ok) {
          const data = await res.json()
          setServerConfig(data)
        } else {
          setConfigError('Could not load server config')
        }
      } catch {
        setConfigError('Could not load server config')
      }
    }
    loadConfig()
  }, [])

  const changeTheme = (t: Theme) => {
    setTheme(t)
    localStorage.setItem(THEME_KEY, t)
    applyTheme(t)
  }

  return (
    <div className="settings-page">
      <h2>Settings</h2>

      <section className="settings-section">
        <h3>User Info</h3>
        {user && (
          <div className="settings-info-grid">
            <div className="settings-row">
              <span className="settings-label">Username</span>
              <span>{user.username}</span>
            </div>
            <div className="settings-row">
              <span className="settings-label">Role</span>
              <span>{user.role}</span>
            </div>
          </div>
        )}
      </section>

      <section className="settings-section">
        <h3>Theme</h3>
        <div className="theme-toggle">
          {(['light', 'dark', 'auto'] as Theme[]).map((t) => (
            <button
              key={t}
              className={`theme-btn ${theme === t ? 'active' : ''}`}
              onClick={() => changeTheme(t)}
            >
              {t.charAt(0).toUpperCase() + t.slice(1)}
            </button>
          ))}
        </div>
      </section>

      <section className="settings-section">
        <h3>Server Configuration</h3>
        {configError && <p className="form-error">{configError}</p>}
        {serverConfig && (
          <div className="settings-info-grid">
            {Object.entries(serverConfig).map(([key, value]) => (
              <div key={key} className="settings-row">
                <span className="settings-label">{key}</span>
                <span>{String(value)}</span>
              </div>
            ))}
          </div>
        )}
        {!serverConfig && !configError && <p>Loading...</p>}
      </section>
    </div>
  )
}
