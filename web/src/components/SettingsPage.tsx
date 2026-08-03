import { useEffect, useState, type ReactNode } from 'react'
import { useAuthStore, authedFetch } from '../store/auth'
import { useResourcesStore } from '../store/resources'
import { useUiStore } from '../store/ui'
import type { Theme } from '../util/themeColor'
import {
  THEME_KEY,
  HUE_KEY,
  ACCENT_PRESETS,
  FONT_SIZES,
  type FontSize,
  type Density,
  type MotionPref,
  getStoredTheme,
  applyTheme,
  getStoredHue,
  applyHue,
  getStoredFontSize,
  setFontSize,
  getStoredDensity,
  setDensity,
  getStoredMotion,
  setMotion,
} from '../util/appearance'
import ClaudeAccountsSection from './ClaudeAccountsSection'
import GrokAccountsSection from './GrokAccountsSection'
import KimiAccountsSection from './KimiAccountsSection'
import ApprovedCommandsSection from './ApprovedCommandsSection'
import SoftwareUpdate from './SoftwareUpdate'
import CustomWorkflowsSection from './CustomWorkflowsSection'
import PluginSettingsForm from './PluginSettingsForm'
import SystemPromptsSection from './SystemPromptsSection'
import ModelPicker from './ModelPicker'
import OllamaPullModel from './OllamaPullModel'
import PluginsSection from './PluginsSection'
import PluginSettingsSection from './PluginSettingsSection'
import PluginRegistryPanel from './PluginRegistryPanel'
import McpServersSection from './McpServersSection'
import RetentionSettingsSection from './RetentionSettingsSection'
import EnvVarsSection from './EnvVarsSection'
import AgentVarsSection from './AgentVarsSection'
import ConfirmDialog from './ConfirmDialog'

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
  | 'workflows'
  | 'plugins'
  | 'plugin-settings'
  | 'providers'
  | 'mcp'
  | 'env'
  | 'variables'
  | 'registry'
  | 'server'

/**
 * The settings hub lists these sub-pages; each groups related sections
 * that used to be stacked on one long page. Plugins (installed plugins,
 * approvals, the registry) is its own sub-page; plugin settings are
 * edited on Plugin Settings (the Ollama and Cursor forms also appear
 * under Providers, same form either way).
 *
 * `adminOnly` sub-pages are hidden from non-admins because everything on
 * them mutates host-wide state (the Claude permission gate, the approved
 * command list, the global MCP server list, the binary itself). The
 * routes behind them are admin-gated server-side — this only keeps the
 * UI honest about what the API will accept.
 */
const SUB_PAGES: { id: SubPage; title: string; blurb: string; adminOnly?: boolean }[] = [
  { id: 'appearance', title: 'Appearance', blurb: 'Theme, accent, text size, density, motion' },
  { id: 'chat', title: 'Chat', blurb: 'Default model, caveman mode and the pre-hatcher model' },
  {
    id: 'prompts',
    title: 'System Prompts',
    blurb: 'Named prompts the cost-aware auto-switch picks from',
  },
  {
    id: 'workflows',
    title: 'Workflows',
    blurb: 'Define custom step sequences for projects and cards',
    adminOnly: true,
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
    adminOnly: true,
  },
  {
    id: 'env',
    title: 'Environment Variables',
    blurb: 'Injected into agent sessions; optionally encrypted with your password',
  },
  {
    id: 'variables',
    title: 'Agent Variables',
    blurb: 'Shared variables agents read and write via tools',
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
    adminOnly: true,
  },
]

/** 16×16 stroke icons for the section rail — one per sub-page, in the
 *  house inline-SVG style (see the app rail buttons). */
const NAV_ICON_PATHS: Record<SubPage, ReactNode> = {
  appearance: (
    <>
      <rect x="2.5" y="2.5" width="4.5" height="4.5" rx="1" />
      <rect x="9" y="2.5" width="4.5" height="4.5" rx="1" />
      <rect x="2.5" y="9" width="4.5" height="4.5" rx="1" />
      <rect x="9" y="9" width="4.5" height="4.5" rx="1" />
    </>
  ),
  chat: (
    <path d="M13.5 8.5a2 2 0 0 1-2 2H7l-3 3v-3h-.5a2 2 0 0 1-2-2v-4a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2z" />
  ),
  prompts: <path d="M3 4h10M3 8h10M3 12h6" />,
  workflows: (
    <>
      <circle cx="4" cy="4" r="1.8" />
      <circle cx="12" cy="8" r="1.8" />
      <circle cx="4" cy="12" r="1.8" />
      <path d="M5.8 4H9a2 2 0 0 1 2 2v.2M5.8 12H9a2 2 0 0 0 2-2v-.2" />
    </>
  ),
  providers: <path d="M6 2v3M10 2v3M4.5 5h7v2.5a3.5 3.5 0 0 1-7 0zM8 11v3" />,
  mcp: (
    <>
      <rect x="2.5" y="3" width="11" height="4" rx="1" />
      <rect x="2.5" y="9" width="11" height="4" rx="1" />
      <path d="M5 5h.01M5 11h.01" />
    </>
  ),
  env: (
    <>
      <circle cx="5.5" cy="10.5" r="3" />
      <path d="M7.8 8.2 13 3M11 5l2 2" />
    </>
  ),
  variables: (
    <path d="M6 2.5c-1.5 0-2 .8-2 2v2c0 .8-.7 1.5-1.5 1.5.8 0 1.5.7 1.5 1.5v2c0 1.2.5 2 2 2M10 2.5c1.5 0 2 .8 2 2v2c0 .8.7 1.5 1.5 1.5-.8 0-1.5.7-1.5 1.5v2c0 1.2-.5 2-2 2" />
  ),
  plugins: (
    <>
      <rect x="2.5" y="2.5" width="11" height="11" rx="2" />
      <path d="M8 5.5v5M5.5 8h5" />
    </>
  ),
  'plugin-settings': (
    <>
      <path d="M3 5h6M13 5h.01M3 11h.01M7 11h6" />
      <circle cx="10.75" cy="5" r="1.75" />
      <circle cx="5.25" cy="11" r="1.75" />
    </>
  ),
  registry: (
    <>
      <path d="M8 2.5 13.5 5v6L8 13.5 2.5 11V5z" />
      <path d="M2.5 5 8 7.5 13.5 5M8 7.5v6" />
    </>
  ),
  server: (
    <>
      <rect x="2.5" y="6" width="11" height="4.5" rx="1" />
      <path d="M4.5 6 6 3.5h4L11.5 6M11.25 8.25h.01" />
    </>
  ),
}

function NavIcon({ id }: { id: SubPage }) {
  return (
    <svg
      width="16"
      height="16"
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
    >
      {NAV_ICON_PATHS[id]}
    </svg>
  )
}

interface Props {
  onBack: () => void
  /** Sub-page to open on mount (e.g. 'plugins' when deep-linked from /plugins). */
  initialSubPage?: SubPage | null
}

export default function SettingsPage({ onBack, initialSubPage = null }: Props) {
  const user = useAuthStore((s) => s.user)
  const isAdmin = user?.role === 'admin'
  const visibleSubPages = SUB_PAGES.filter((p) => isAdmin || !p.adminOnly)
  const [subPage, setSubPage] = useState<SubPage | null>(initialSubPage)
  // A non-admin must not land on — or stay on — an admin-only sub-page, even
  // through the `initialSubPage` deep link. Everything below renders off
  // `activeSubPage`, so an out-of-reach id falls back to the hub.
  const activeSubPage = visibleSubPages.some((p) => p.id === subPage) ? subPage : null
  const [theme, setTheme] = useState<Theme>(getStoredTheme)
  const [hue, setHue] = useState<number>(getStoredHue)
  const [fontSize, setFontSizeState] = useState<FontSize>(getStoredFontSize)
  const [density, setDensityState] = useState<Density>(getStoredDensity)
  const [motion, setMotionState] = useState<MotionPref>(getStoredMotion)
  const skipBacklogConfirm = useUiStore((s) => s.skipBacklogConfirm)
  const setSkipBacklogConfirm = useUiStore((s) => s.setSkipBacklogConfirm)
  const [serverConfig, setServerConfig] = useState<ServerConfig | null>(null)
  const [caveman, setCaveman] = useState<string>('off')
  const [claudeBypass, setClaudeBypass] = useState<boolean>(false)
  // Enforced → Bypass is a host-wide loosening of the permission gate, so it
  // goes through a confirm. The other direction (back to Enforced) is safe
  // and stays a single click.
  const [confirmBypass, setConfirmBypass] = useState(false)
  const [preHatchModel, setPreHatchModel] = useState<string>('')
  const models = useResourcesStore((s) => s.models)
  const fetchModels = useResourcesStore((s) => s.fetchModels)
  const defaultModel = useResourcesStore((s) => s.defaultModel)
  const fetchDefaultModel = useResourcesStore((s) => s.fetchDefaultModel)
  const setDefaultModelLocal = useResourcesStore((s) => s.setDefaultModelLocal)
  const [providerVisibility, setProviderVisibility] = useState<ProviderInfo[]>([])
  const [backupStatus, setBackupStatus] = useState<BackupStatus | null>(null)
  // One inline save error at a time, tagged with the section that owns the
  // control. A failed PUT reverts the control and parks the reason here.
  const [saveError, setSaveError] = useState<{ scope: string; message: string } | null>(null)

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
    // Admin-only route (it reads the host-wide permission gate), and the
    // only consumer is the Server sub-page plus its nav badge — both hidden
    // from non-admins.
    if (!isAdmin) return
    authedFetch('/api/settings/claude-permissions')
      .then((res) => (res.ok ? res.json() : null))
      .then((data: { bypass?: boolean } | null) => {
        if (typeof data?.bypass === 'boolean') setClaudeBypass(data.bypass)
      })
      .catch(() => {})
  }, [isAdmin])

  useEffect(() => {
    fetchModels()
  }, [fetchModels])

  useEffect(() => {
    fetchDefaultModel()
  }, [fetchDefaultModel])

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
  /**
   * PUT one settings value, rejecting with a readable message on any
   * non-2xx or network failure. Callers revert their optimistic state in
   * the rejection handler — a save the server refused must never be left
   * on screen as if it stuck.
   */
  const putSetting = async (url: string, body: unknown, fallback: string): Promise<void> => {
    let res: Response
    try {
      res = await authedFetch(url, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      })
    } catch {
      throw new Error(`${fallback}: network error`)
    }
    if (!res.ok) {
      const data = (await res.json().catch(() => null)) as { error?: unknown } | null
      const detail = typeof data?.error === 'string' ? data.error : `HTTP ${res.status}`
      throw new Error(`${fallback}: ${detail}`)
    }
  }

  const changeCaveman = (level: string) => {
    const prev = caveman
    setCaveman(level)
    setSaveError(null)
    void putSetting('/api/settings/caveman', { level }, 'Could not save caveman mode').catch(
      (e: Error) => {
        setCaveman(prev)
        setSaveError({ scope: 'caveman', message: e.message })
      },
    )
  }

  const changeClaudeBypass = (bypass: boolean) => {
    const prev = claudeBypass
    setClaudeBypass(bypass)
    setSaveError(null)
    void putSetting(
      '/api/settings/claude-permissions',
      { bypass },
      'Could not save permission mode',
    ).catch((e: Error) => {
      setClaudeBypass(prev)
      setSaveError({ scope: 'claude-permissions', message: e.message })
    })
  }

  const changePreHatchModel = (model: string) => {
    const prev = preHatchModel
    setPreHatchModel(model)
    setSaveError(null)
    void putSetting(
      '/api/settings/pre-hatcher',
      { model },
      'Could not save pre-hatcher model',
    ).catch((e: Error) => {
      setPreHatchModel(prev)
      setSaveError({ scope: 'prehatch', message: e.message })
    })
  }

  const changeDefaultModel = (model: string) => {
    const prev = defaultModel
    setDefaultModelLocal(model)
    setSaveError(null)
    void putSetting('/api/settings/default-model', { model }, 'Could not save default model').catch(
      (e: Error) => {
        setDefaultModelLocal(prev)
        setSaveError({ scope: 'default-model', message: e.message })
      },
    )
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

  const changeFontSize = (size: FontSize) => {
    setFontSizeState(size)
    setFontSize(size)
  }

  const changeDensity = (d: Density) => {
    setDensityState(d)
    setDensity(d)
  }

  const changeMotion = (m: MotionPref) => {
    setMotionState(m)
    setMotion(m)
  }

  const toggleProvider = (id: string, hidden: boolean) => {
    setSaveError(null)
    // `providerVisibility` is only ever set from a server read, so a failed
    // PUT leaves the checkbox showing the server's state on re-render.
    void putSetting(`/api/settings/providers/${id}`, { hidden }, 'Could not update provider')
      .then(() => {
        authedFetch('/api/settings/providers')
          .then((r) => (r.ok ? r.json() : null))
          .then((data: { providers?: ProviderInfo[] } | null) => {
            if (data?.providers) setProviderVisibility(data.providers)
          })
          .catch(() => {})
        fetchModels()
      })
      .catch((e: Error) => setSaveError({ scope: 'providers', message: e.message }))
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

  const current = visibleSubPages.find((p) => p.id === activeSubPage)

  return (
    <div className="settings-page" data-testid="settings-page" data-sub={activeSubPage ?? 'none'}>
      <div className="settings-page-header">
        <button
          type="button"
          className="btn-secondary settings-back"
          onClick={() => (activeSubPage ? setSubPage(null) : onBack())}
        >
          ← Back
        </button>
        <h2>{current ? `Settings · ${current.title}` : 'Settings'}</h2>
      </div>

      <div className="settings-content">
        {activeSubPage === null && (
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
        )}

        {activeSubPage === 'appearance' && (
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

            <section className="settings-section" data-testid="accent-section">
              <h3>Accent Color</h3>
              <p className="form-hint">
                Colors buttons, links, focus rings and the running-agent glow. Pick a preset or dial
                in any hue.
              </p>
              <div className="accent-swatches" role="group" aria-label="Accent presets">
                {ACCENT_PRESETS.map((p) => (
                  <button
                    key={p.name}
                    type="button"
                    className={`accent-swatch ${hue === p.hue ? 'active' : ''}`}
                    style={{ backgroundColor: `hsl(${p.hue}, 72%, 50%)` }}
                    title={p.name}
                    aria-label={`${p.name} accent`}
                    aria-pressed={hue === p.hue}
                    data-testid={`accent-swatch-${p.hue}`}
                    onClick={() => changeHue(p.hue)}
                  />
                ))}
              </div>
              <div className="settings-hue">
                <input
                  aria-label="Accent hue"
                  type="range"
                  min={0}
                  max={360}
                  value={hue}
                  onChange={(e) => changeHue(parseInt(e.target.value, 10))}
                  className="hue-slider"
                />
                <span className="hue-value">{hue}</span>
                <span
                  className="hue-preview"
                  style={{ backgroundColor: `hsl(${hue}, 72%, 50%)` }}
                />
              </div>
            </section>

            <section className="settings-section" data-testid="font-size-section">
              <h3>Font Size</h3>
              <p className="form-hint">
                Scales all interface text. Default follows your browser&apos;s setting.
              </p>
              <div className="theme-toggle">
                {FONT_SIZES.map((f) => (
                  <button
                    key={f.id}
                    className={`theme-btn ${fontSize === f.id ? 'active' : ''}`}
                    data-testid={`font-size-${f.id}`}
                    onClick={() => changeFontSize(f.id)}
                  >
                    {f.label}
                  </button>
                ))}
              </div>
            </section>

            <section className="settings-section" data-testid="density-section">
              <h3>Density</h3>
              <p className="form-hint">
                Compact tightens spacing in chat, lists and settings to fit more on screen.
              </p>
              <div className="theme-toggle">
                {(['comfortable', 'compact'] as Density[]).map((d) => (
                  <button
                    key={d}
                    className={`theme-btn ${density === d ? 'active' : ''}`}
                    data-testid={`density-${d}`}
                    onClick={() => changeDensity(d)}
                  >
                    {d === 'comfortable' ? 'Comfortable' : 'Compact'}
                  </button>
                ))}
              </div>
            </section>

            <section className="settings-section" data-testid="motion-section">
              <h3>Motion</h3>
              <p className="form-hint">
                Reduced collapses animations and transitions; anything signalled only by motion
                keeps a static indicator instead. System follows your OS preference.
              </p>
              <div className="theme-toggle">
                {(['system', 'reduced'] as MotionPref[]).map((m) => (
                  <button
                    key={m}
                    className={`theme-btn ${motion === m ? 'active' : ''}`}
                    data-testid={`motion-${m}`}
                    onClick={() => changeMotion(m)}
                  >
                    {m === 'system' ? 'System' : 'Reduced'}
                  </button>
                ))}
              </div>
            </section>

            <section className="settings-section" data-testid="confirmations-section">
              <h3>Confirmations</h3>
              <p className="form-hint">
                Moving a card out of Backlog starts a paid worker and locks the card&apos;s
                description and workflow. Re-enable the warning here if you dismissed it with
                &ldquo;Don&apos;t ask again&rdquo;.
              </p>
              <div className="settings-info-grid">
                <label className="settings-row settings-row-toggle">
                  <input
                    type="checkbox"
                    checked={!skipBacklogConfirm}
                    data-testid="backlog-confirm-toggle"
                    onChange={(e) => setSkipBacklogConfirm(!e.target.checked)}
                  />
                  <span className="settings-label">
                    Confirm before starting work on a Backlog card
                  </span>
                </label>
              </div>
            </section>
          </>
        )}

        {activeSubPage === 'chat' && (
          <>
            <section className="settings-section" data-testid="default-model-section">
              <h3>Default Model</h3>
              <p className="form-hint">
                Preselected for new sessions, cards, and reviews, and used for anything dispatched
                without an explicit model. Until one is chosen, PeckBoard routes by effort
                (low→Haiku, medium→Sonnet, high→Opus, higher→Fable).
              </p>
              <ModelPicker
                value={defaultModel}
                onChange={changeDefaultModel}
                models={models}
                ariaLabel="Default model"
                testId="default-model"
                emptyHint="Loading models…"
                onOpen={fetchModels}
              />
              {saveError?.scope === 'default-model' && (
                <p className="form-error" role="alert" data-testid="settings-error-default-model">
                  {saveError.message}
                </p>
              )}
            </section>
            <section className="settings-section" data-testid="caveman-section">
              <h3>Caveman Mode</h3>
              <p className="form-hint">
                Terse agent replies in chat sessions — cuts output tokens (roughly 65% at Full)
                while keeping code and technical content exact. Workers are always terse. Applies
                from each session&apos;s next message.
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
              {saveError?.scope === 'caveman' && (
                <p className="form-error" role="alert" data-testid="settings-error-caveman">
                  {saveError.message}
                </p>
              )}
            </section>

            <section className="settings-section" data-testid="prehatch-section">
              <h3>Pre-hatcher Model</h3>
              <p className="form-hint">
                The model the pre-hatcher plugin researches on before a chat message reaches the
                main model. Auto uses the session provider&apos;s cheapest priced model. Applies
                from the next message.
              </p>
              <ModelPicker
                value={preHatchModel}
                onChange={changePreHatchModel}
                models={models}
                defaultLabel="Auto — provider's cheapest model"
                ariaLabel="Pre-hatcher model"
                testId="prehatch-model"
                emptyHint="Loading models…"
                onOpen={fetchModels}
              />
              {saveError?.scope === 'prehatch' && (
                <p className="form-error" role="alert" data-testid="settings-error-prehatch">
                  {saveError.message}
                </p>
              )}
            </section>
          </>
        )}

        {activeSubPage === 'providers' && (
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
                    <label className="settings-row" key={p.id}>
                      <span className="settings-label">{p.display_name}</span>
                      <input
                        type="checkbox"
                        checked={!p.hidden}
                        data-testid={`provider-toggle-${p.id}`}
                        onChange={(e) => toggleProvider(p.id, !e.target.checked)}
                      />
                    </label>
                  ))}
                </div>
              )}
              {saveError?.scope === 'providers' && (
                <p className="form-error" role="alert" data-testid="settings-error-providers">
                  {saveError.message}
                </p>
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
                    <p className="settings-loading">
                      No login has been kept alive yet this session.
                    </p>
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

        {activeSubPage === 'server' && (
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

            <section className="settings-section" data-testid="claude-permissions-section">
              <h3>Claude Tool Permissions</h3>
              <p className="form-hint">
                Enforced (default) runs Claude CLI sessions under PeckBoard&apos;s permission gate:
                every tool call is checked server-side, file access outside the project folder is
                denied, and the terminal tool stays blocked. Bypass restores the legacy
                --dangerously-skip-permissions behavior for this host. Applies to newly spawned
                agent processes.
              </p>
              <div className="theme-toggle">
                <button
                  className={`theme-btn ${!claudeBypass ? 'active' : ''}`}
                  onClick={() => changeClaudeBypass(false)}
                  data-testid="claude-permissions-enforced"
                >
                  Enforced
                </button>
                <button
                  className={`theme-btn ${claudeBypass ? 'active' : ''}`}
                  onClick={() => (claudeBypass ? changeClaudeBypass(true) : setConfirmBypass(true))}
                  data-testid="claude-permissions-bypass"
                >
                  Bypass
                </button>
              </div>
              {saveError?.scope === 'claude-permissions' && (
                <p
                  className="form-error"
                  role="alert"
                  data-testid="settings-error-claude-permissions"
                >
                  {saveError.message}
                </p>
              )}
            </section>

            {confirmBypass && (
              <ConfirmDialog
                testId="claude-bypass-confirm"
                danger
                title="Turn off the permission gate for this host?"
                message="Newly spawned Claude agents run with --dangerously-skip-permissions: tool calls are no longer checked server-side, file reads and writes outside the project folder are allowed, and the terminal tool is unblocked. It applies to every project and every user on this host until someone sets it back to Enforced."
                confirmLabel="Bypass permissions"
                cancelLabel="Keep enforced"
                onConfirm={() => {
                  setConfirmBypass(false)
                  changeClaudeBypass(true)
                }}
                onCancel={() => setConfirmBypass(false)}
              />
            )}

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

            {user?.role === 'admin' && <RetentionSettingsSection />}
          </>
        )}

        {activeSubPage === 'mcp' && <McpServersSection />}
        {activeSubPage === 'env' && <EnvVarsSection />}
        {activeSubPage === 'variables' && <AgentVarsSection />}
        {activeSubPage === 'plugins' && (
          <PluginsSection onBrowseRegistry={() => setSubPage('registry')} />
        )}
        {activeSubPage === 'plugin-settings' && <PluginSettingsSection />}
        {activeSubPage === 'registry' && (
          <PluginRegistryPanel onManagePlugins={() => setSubPage('plugins')} />
        )}
        {activeSubPage === 'prompts' && <SystemPromptsSection />}
        {activeSubPage === 'workflows' && <CustomWorkflowsSection />}
      </div>

      <nav className="settings-nav" aria-label="Settings sections">
        {visibleSubPages.map((p) => (
          <button
            key={p.id}
            type="button"
            className="settings-nav-item"
            data-testid={`settings-nav-${p.id}`}
            aria-current={activeSubPage === p.id ? 'true' : undefined}
            onClick={() => setSubPage(p.id)}
          >
            <span className="settings-nav-icon">
              <NavIcon id={p.id} />
            </span>
            <span className="settings-nav-title">
              {p.title}
              {p.id === 'server' && claudeBypass && (
                <span className="settings-nav-badge" data-testid="settings-bypass-badge">
                  Tool permissions bypassed
                </span>
              )}
            </span>
            <span className="settings-nav-blurb">{p.blurb}</span>
            <span className="settings-nav-chevron" aria-hidden>
              ›
            </span>
          </button>
        ))}
      </nav>
    </div>
  )
}
