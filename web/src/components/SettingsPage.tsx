import { useEffect, useState } from 'react'
import { useAuthStore, authedFetch } from '../store/auth'
import { useResourcesStore } from '../store/resources'
import type { Theme } from '../util/themeColor'
import {
  THEME_KEY,
  HUE_KEY,
  getStoredTheme,
  applyTheme,
  getStoredHue,
  applyHue,
} from '../util/appearance'
import ClaudeAccountsSection from './ClaudeAccountsSection'
import GrokAccountsSection from './GrokAccountsSection'
import KimiAccountsSection from './KimiAccountsSection'
import ApprovedCommandsSection from './ApprovedCommandsSection'
import SoftwareUpdate from './SoftwareUpdate'
import PluginSettingsForm from './PluginSettingsForm'
import SystemPromptsSection from './SystemPromptsSection'
import OllamaPullModel from './OllamaPullModel'
import PluginsSection from './PluginsSection'
import PluginSettingsSection from './PluginSettingsSection'
import PluginRegistryPanel from './PluginRegistryPanel'
import McpServersSection from './McpServersSection'

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

interface BackupStatus {
  scheduled: boolean
  intervalHours: number | null
  dir: string | null
  retention: number | null
}

interface ProviderInfo {
  id: string
  display_name: string
  hidden: boolean
}

function formatInterval(hours: number): string {
  if (hours === 0) return 'Keep-alive is disabled.'
  return hours === 1 ? 'Runs every hour.' : `Runs every ${hours} hours.`
}

function formatWhen(at: string): string {
  const d = new Date(at)
  return isNaN(d.getTime()) ? at : d.toLocaleString()
}
type SubPage =
  | 'appearance'
  | 'chat'
  | 'prompts'
  | 'plugins'
  | 'plugin-settings'
  | 'providers'
  | 'mcp'
  | 'registry'
  | 'server'

/**
 * The settings hub lists these sub-pages; each groups related sections
 * that used to be stacked on one long page. Plugins (installed plugins,
 * approvals, the registry) is its own sub-page; plugin settings are
 * edited on Plugin Settings (the Ollama and Cursor forms also appear
 * under Providers, same form either way).
 */
const SUB_PAGES: { id: SubPage; title: string; blurb: string }[] = [
  { id: 'appearance', title: 'Appearance', blurb: 'Theme and accent color' },
  { id: 'chat', title: 'Chat', blurb: 'Caveman mode and the pre-hatcher model' },
  {
    id: 'prompts',
    title: 'System Prompts',
    blurb: 'Named prompts the cost-aware auto-switch picks from',
  },
  {
    id: 'providers',
    title: 'Providers & Accounts',
    blurb: 'Claude, Grok and Kimi accounts, Ollama servers, Cursor CLI, keep-alive',
  },
  {
    id: 'mcp',
    title: 'MCP Servers',
    blurb: 'External tool servers injected into agent sessions',
  },
  {
    id: 'plugins',
    title: 'Plugins',
    blurb: 'Installed plugins and approvals',
  },
  {
    id: 'plugin-settings',
    title: 'Plugin Settings',
    blurb: 'Configure plugins that declare settings',
  },
  {
    id: 'registry',
    title: 'Plugin Registry',
    blurb: 'Browse and install plugins, manage registry repositories',
  },
  {
    id: 'server',
    title: 'Server',
    blurb: 'Ports, data directory, approved commands, software updates',
  },
]

interface Props {
  onBack: () => void
  /** Sub-page to open on mount (e.g. 'plugins' when deep-linked from /plugins). */
  initialSubPage?: SubPage | null
}

export default function SettingsPage({ onBack, initialSubPage = null }: Props) {
  const user = useAuthStore((s) => s.user)
  const [subPage, setSubPage] = useState<SubPage | null>(initialSubPage)
  const [theme, setTheme] = useState<Theme>(getStoredTheme)
  const [hue, setHue] = useState<number>(getStoredHue)
  const [serverConfig, setServerConfig] = useState<ServerConfig | null>(null)
  const [caveman, setCaveman] = useState<string>('off')
  const [preHatchModel, setPreHatchModel] = useState<string>('')
  const models = useResourcesStore((s) => s.models)
  const providers = useResourcesStore((s) => s.providers)
  const fetchModels = useResourcesStore((s) => s.fetchModels)
  const [providerVisibility, setProviderVisibility] = useState<ProviderInfo[]>([])
  const [backupStatus, setBackupStatus] = useState<BackupStatus | null>(null)

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

  useEffect(() => {
    authedFetch('/api/settings/providers')
      .then((res) => (res.ok ? res.json() : null))
      .then((data: { providers?: ProviderInfo[] } | null) => {
        if (data?.providers) setProviderVisibility(data.providers)
      })
      .catch(() => {})
  }, [])

  useEffect(() => {
    if (user?.role !== 'admin') return
    authedFetch('/api/admin/backup/status')
      .then((res) => (res.ok ? res.json() : null))
      .then((data: BackupStatus | null) => {
        if (data) setBackupStatus(data)
      })
      .catch(() => {})
  }, [user])
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

  const toggleProvider = (id: string, hidden: boolean) => {
    authedFetch(`/api/settings/providers/${id}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ hidden }),
    })
      .then((res) => {
        if (!res.ok) return
        authedFetch('/api/settings/providers')
          .then((r) => (r.ok ? r.json() : null))
          .then((data: { providers?: ProviderInfo[] } | null) => {
            if (data?.providers) setProviderVisibility(data.providers)
          })
          .catch(() => {})
        fetchModels()
      })
      .catch(() => {})
  }

  const downloadBackup = async () => {
    try {
      const res = await authedFetch('/api/admin/backup')
      if (!res.ok) return
      const blob = await res.blob()
      const url = URL.createObjectURL(blob)
      const a = document.createElement('a')
      a.href = url
      a.download = `peckboard-backup-${Date.now()}.tar.gz`
      document.body.appendChild(a)
      a.click()
      document.body.removeChild(a)
      URL.revokeObjectURL(url)
    } catch {
      // silently ignore
    }
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
          <section className="settings-section">
            <h3>Providers</h3>
            <p className="form-hint">
              Toggle providers on or off. Hidden providers are removed from model pickers and
              account settings.
            </p>
            {providerVisibility.length === 0 ? (
              <p className="settings-loading">Loading providers...</p>
            ) : (
              <div className="settings-info-grid">
                {providerVisibility.map((p) => (
                  <div className="settings-row" key={p.id}>
                    <span className="settings-label">{p.display_name}</span>
                    <input
                      type="checkbox"
                      checked={!p.hidden}
                      data-testid={`provider-toggle-${p.id}`}
                      onChange={(e) => toggleProvider(p.id, !e.target.checked)}
                    />
                  </div>
                ))}
              </div>
            )}
          </section>

          {providerVisibility.find((p) => p.id === 'claude')?.hidden !== true && (
            <ClaudeAccountsSection />
          )}

          {providerVisibility.find((p) => p.id === 'grok')?.hidden !== true && (
            <GrokAccountsSection />
          )}

          {providerVisibility.find((p) => p.id === 'kimi')?.hidden !== true && (
            <KimiAccountsSection />
          )}

          {providerVisibility.find((p) => p.id === 'ollama')?.hidden !== true && (
            <section className="settings-section" data-testid="ollama-settings-section">
              <h3>Ollama</h3>
              <p className="form-hint">
                Local and remote Ollama servers. Models on the default server appear under their
                bare name; models on additional named servers appear as model@server (e.g.
                qwen2.5-coder@gpu-box).
              </p>
              <PluginSettingsForm pluginId="ollama" />
              <OllamaPullModel />
            </section>
          )}

          {providerVisibility.find((p) => p.id === 'cursor')?.hidden !== true && (
            <section className="settings-section" data-testid="cursor-settings-section">
              <h3>Cursor</h3>
              <p className="form-hint">
                The cursor-agent CLI provider: binary path, default model, and model discovery.
              </p>
              <PluginSettingsForm pluginId="cursor" />
            </section>
          )}

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

          {user?.role === 'admin' && (
            <section className="settings-section" data-testid="backup-section">
              <h3>Backup</h3>
              <p className="form-hint">
                Download a consistent snapshot of your database, config, reports, attachments, and
                plugins.
              </p>
              <div className="settings-row">
                <button
                  type="button"
                  className="btn-secondary"
                  data-testid="backup-download-btn"
                  onClick={downloadBackup}
                >
                  Download backup
                </button>
              </div>
              {backupStatus?.scheduled && (
                <p className="form-hint">
                  Scheduled: every {backupStatus.intervalHours}h → {backupStatus.dir} (keep{' '}
                  {backupStatus.retention})
                </p>
              )}
            </section>
          )}
        </>
      )}

      {subPage === 'mcp' && <McpServersSection />}
      {subPage === 'plugins' && <PluginsSection onBrowseRegistry={() => setSubPage('registry')} />}
      {subPage === 'plugin-settings' && <PluginSettingsSection />}
      {subPage === 'registry' && <PluginRegistryPanel />}
      {subPage === 'prompts' && <SystemPromptsSection />}
    </div>
  )
}
