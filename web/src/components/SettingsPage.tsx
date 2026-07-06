import { useEffect, useState } from 'react'
import { useAuthStore, authedFetch } from '../store/auth'
import { useResourcesStore } from '../store/resources'
import { applyThemeColor, type Theme } from '../util/themeColor'
import ClaudeAccountsSection from './ClaudeAccountsSection'
import GrokAccountsSection from './GrokAccountsSection'
import ApprovedCommandsSection from './ApprovedCommandsSection'
import SoftwareUpdate from './SoftwareUpdate'
import PluginSettingsForm from './PluginSettingsForm'
import OllamaPullModel from './OllamaPullModel'

const THEME_KEY = 'peckboard_theme'
const HUE_KEY = 'peckboard_hue'

interface KeepAliveRun {
  provider: string
  account_id: string | null
  label: string
  at: string
}

interface ServerConfig {
  port: number
  https_port: number
  host: string
  data_dir: string
  keep_alive_hours: number
  keepalive_last_runs: KeepAliveRun[]
}

function formatInterval(hours: number): string {
  if (hours === 0) return 'Keep-alive is disabled.'
  return hours === 1 ? 'Runs every hour.' : `Runs every ${hours} hours.`
}

function formatWhen(at: string): string {
  const d = new Date(at)
  return isNaN(d.getTime()) ? at : d.toLocaleString()
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

type SubPage = 'appearance' | 'chat' | 'providers' | 'server'

/**
 * The settings hub lists these sub-pages; each groups related sections
 * that used to be stacked on one long page. Providers is where the
 * Ollama and Cursor plugin settings now live (they're also reachable
 * from the Plugins modal, same form either way).
 */
const SUB_PAGES: { id: SubPage; title: string; blurb: string }[] = [
  { id: 'appearance', title: 'Appearance', blurb: 'Theme and accent color' },
  { id: 'chat', title: 'Chat', blurb: 'Caveman mode and the pre-hatcher model' },
  {
    id: 'providers',
    title: 'Providers & Accounts',
    blurb: 'Claude and Grok accounts, Ollama servers, Cursor CLI, keep-alive',
  },
  {
    id: 'server',
    title: 'Server',
    blurb: 'Ports, data directory, approved commands, software updates',
  },
]

interface Props {
  onBack: () => void
}

export default function SettingsPage({ onBack }: Props) {
  const user = useAuthStore((s) => s.user)
  const [subPage, setSubPage] = useState<SubPage | null>(null)
  const [theme, setTheme] = useState<Theme>(getStoredTheme)
  const [hue, setHue] = useState<number>(() => {
    const stored = getStoredHue()
    applyHue(stored)
    return stored
  })
  const [serverConfig, setServerConfig] = useState<ServerConfig | null>(null)
  const [caveman, setCaveman] = useState<string>('off')
  const [preHatchModel, setPreHatchModel] = useState<string>('')
  const models = useResourcesStore((s) => s.models)
  const providers = useResourcesStore((s) => s.providers)
  const fetchModels = useResourcesStore((s) => s.fetchModels)

  useEffect(() => {
    authedFetch('/api/config')
      .then((res) => (res.ok ? res.json() : null))
      .then((data: ServerConfig | null) => {
        if (data) setServerConfig(data)
      })
      .catch(() => {})
  }, [])

  useEffect(() => {
    authedFetch('/api/settings/caveman')
      .then((res) => (res.ok ? res.json() : null))
      .then((data: { level?: string } | null) => {
        if (data?.level) setCaveman(data.level)
      })
      .catch(() => {})
  }, [])

  useEffect(() => {
    fetchModels()
  }, [fetchModels])

  useEffect(() => {
    authedFetch('/api/settings/pre-hatcher')
      .then((res) => (res.ok ? res.json() : null))
      .then((data: { model?: string } | null) => {
        if (typeof data?.model === 'string') setPreHatchModel(data.model)
      })
      .catch(() => {})
  }, [])
  const changeCaveman = (level: string) => {
    setCaveman(level)
    authedFetch('/api/settings/caveman', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ level }),
    }).catch(() => {})
  }

  const changePreHatchModel = (model: string) => {
    setPreHatchModel(model)
    authedFetch('/api/settings/pre-hatcher', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ model }),
    }).catch(() => {})
  }
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

  const current = SUB_PAGES.find((p) => p.id === subPage)

  return (
    <div className="settings-page" data-testid="settings-page">
      <div className="settings-page-header">
        <button
          type="button"
          className="btn-secondary settings-back"
          onClick={() => (subPage ? setSubPage(null) : onBack())}
        >
          ← Back
        </button>
        <h2>{current ? `Settings · ${current.title}` : 'Settings'}</h2>
      </div>

      {subPage === null && (
        <>
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

          <nav className="settings-nav" aria-label="Settings sections">
            {SUB_PAGES.map((p) => (
              <button
                key={p.id}
                type="button"
                className="settings-nav-item"
                data-testid={`settings-nav-${p.id}`}
                onClick={() => setSubPage(p.id)}
              >
                <span className="settings-nav-title">{p.title}</span>
                <span className="settings-nav-blurb">{p.blurb}</span>
                <span className="settings-nav-chevron" aria-hidden>
                  ›
                </span>
              </button>
            ))}
          </nav>
        </>
      )}

      {subPage === 'appearance' && (
        <>
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
        </>
      )}

      {subPage === 'chat' && (
        <>
          <section className="settings-section" data-testid="caveman-section">
            <h3>Caveman Mode</h3>
            <p className="form-hint">
              Terse agent replies in chat sessions — cuts output tokens (roughly 65% at Full) while
              keeping code and technical content exact. Workers are always terse. Applies from each
              session&apos;s next message.
            </p>
            <div className="theme-toggle">
              {['off', 'lite', 'full'].map((l) => (
                <button
                  key={l}
                  className={`theme-btn ${caveman === l ? 'active' : ''}`}
                  onClick={() => changeCaveman(l)}
                >
                  {l.charAt(0).toUpperCase() + l.slice(1)}
                </button>
              ))}
            </div>
          </section>

          <section className="settings-section" data-testid="prehatch-section">
            <h3>Pre-hatcher Model</h3>
            <p className="form-hint">
              The model the pre-hatcher plugin researches on before a chat message reaches the main
              model. Auto uses the session provider&apos;s cheapest priced model. Applies from the
              next message.
            </p>
            <select
              className="form-input"
              value={preHatchModel}
              onChange={(e) => changePreHatchModel(e.target.value)}
              data-testid="prehatch-model-select"
            >
              <option value="">Auto — provider&apos;s cheapest model</option>
              {preHatchModel !== '' && !models.some((m) => m.id === preHatchModel) && (
                <option value={preHatchModel}>{preHatchModel}</option>
              )}
              {providers.length > 0
                ? providers.map((p) => (
                    <optgroup key={p.id} label={p.display_name}>
                      {p.models.map((m) => (
                        <option key={m.id} value={m.id}>
                          {m.display_name}
                        </option>
                      ))}
                    </optgroup>
                  ))
                : models.map((m) => (
                    <option key={m.id} value={m.id}>
                      {m.display_name}
                    </option>
                  ))}
            </select>
          </section>
        </>
      )}

      {subPage === 'providers' && (
        <>
          <ClaudeAccountsSection />

          <GrokAccountsSection />

          <section className="settings-section" data-testid="ollama-settings-section">
            <h3>Ollama</h3>
            <p className="form-hint">
              Local and remote Ollama servers. Models on the default server appear under their bare
              name; models on additional named servers appear as model@server (e.g.
              qwen2.5-coder@gpu-box).
            </p>
            <PluginSettingsForm pluginId="ollama" />
            <OllamaPullModel />
          </section>

          <section className="settings-section" data-testid="cursor-settings-section">
            <h3>Cursor</h3>
            <p className="form-hint">
              The cursor-agent CLI provider: binary path, default model, and model discovery.
            </p>
            <PluginSettingsForm pluginId="cursor" />
          </section>

          <section className="settings-section" data-testid="keepalive-section">
            <h3>Provider Keep-Alive</h3>
            {serverConfig ? (
              <>
                <p className="form-hint">
                  {formatInterval(serverConfig.keep_alive_hours)} Each provider login — the host
                  default and every account — is pinged with a throwaway message so its token
                  doesn&apos;t go stale.
                </p>
                {serverConfig.keepalive_last_runs.length === 0 ? (
                  <p className="settings-loading">No login has been kept alive yet this session.</p>
                ) : (
                  <div className="settings-info-grid">
                    {serverConfig.keepalive_last_runs.map((r) => (
                      <div
                        className="settings-row"
                        key={`${r.provider}:${r.account_id ?? 'default'}`}
                      >
                        <span className="settings-label">{r.label}</span>
                        <span>{formatWhen(r.at)}</span>
                      </div>
                    ))}
                  </div>
                )}
              </>
            ) : (
              <p className="settings-loading">Loading keep-alive status...</p>
            )}
          </section>
        </>
      )}

      {subPage === 'server' && (
        <>
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

          <ApprovedCommandsSection />

          <SoftwareUpdate />
        </>
      )}
    </div>
  )
}
