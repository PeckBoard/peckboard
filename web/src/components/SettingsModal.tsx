import { useEffect, useState } from 'react'
import { useAuthStore, authedFetch } from '../store/auth'
import { applyThemeColor, type Theme } from '../util/themeColor'
import ClaudeAccountsSection from './ClaudeAccountsSection'
import Modal from './Modal'
import SoftwareUpdate from './SoftwareUpdate'

const THEME_KEY = 'peckboard_theme'
const HUE_KEY = 'peckboard_hue'

interface ServerConfig {
  port: number
  https_port: number
  host: string
  data_dir: string
}

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
  applyThemeColor(theme)
}

function getStoredHue(): number {
  const stored = localStorage.getItem(HUE_KEY)
  if (stored !== null) {
    const n = parseInt(stored, 10)
    if (!isNaN(n) && n >= 0 && n <= 360) return n
  }
  return 220
}

function applyHue(hue: number) {
  document.documentElement.style.setProperty('--primary-hue', String(hue))
}

interface Props {
  onClose: () => void
}

export default function SettingsModal({ onClose }: Props) {
  const user = useAuthStore((s) => s.user)
  const [theme, setTheme] = useState<Theme>(getStoredTheme)
  const [hue, setHue] = useState<number>(() => {
    const stored = getStoredHue()
    applyHue(stored)
    return stored
  })
  const [serverConfig, setServerConfig] = useState<ServerConfig | null>(null)

  useEffect(() => {
    authedFetch('/api/config')
      .then((res) => (res.ok ? res.json() : null))
      .then((data: ServerConfig | null) => {
        if (data) setServerConfig(data)
      })
      .catch(() => {})
  }, [])

  const changeTheme = (t: Theme) => {
    setTheme(t)
    localStorage.setItem(THEME_KEY, t)
    applyTheme(t)
  }

  const changeHue = (newHue: number) => {
    setHue(newHue)
    localStorage.setItem(HUE_KEY, String(newHue))
    applyHue(newHue)
  }

  return (
    <Modal onClose={onClose} className="settings-modal" data-testid="settings-modal">
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
        <h3>Server</h3>
        {serverConfig ? (
          <div className="settings-info-grid">
            <div className="settings-row">
              <span className="settings-label">HTTP Port</span>
              <span>{serverConfig.port}</span>
            </div>
            <div className="settings-row">
              <span className="settings-label">HTTPS Port</span>
              <span>{serverConfig.https_port}</span>
            </div>
            <div className="settings-row">
              <span className="settings-label">Data Directory</span>
              <span>{serverConfig.data_dir}</span>
            </div>
          </div>
        ) : (
          <p className="settings-loading">Loading server config...</p>
        )}
      </section>

      <ClaudeAccountsSection />

      <SoftwareUpdate />

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
        <h3>Accent Hue</h3>
        <div className="settings-hue">
          <input
            type="range"
            min={0}
            max={360}
            value={hue}
            onChange={(e) => changeHue(parseInt(e.target.value, 10))}
            className="hue-slider"
          />
          <span className="hue-value">{hue}</span>
          <span className="hue-preview" style={{ backgroundColor: `hsl(${hue}, 72%, 50%)` }} />
        </div>
      </section>

      <div className="form-actions">
        <button type="button" className="btn-secondary" onClick={onClose}>
          Close
        </button>
      </div>
    </Modal>
  )
}
