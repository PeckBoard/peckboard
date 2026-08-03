import { useEffect, useState, type ReactNode } from 'react'
import { useAuthStore, authedFetch } from '../store/auth'
import { useResourcesStore } from '../store/resources'
import { useUiStore } from '../store/ui'
import { useMediaQuery } from '../hooks/useMediaQuery'
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
import ChangePasswordModal from './ChangePasswordModal'
import UserManagement from './UserManagement'
import PluginRegistryPanel from './PluginRegistryPanel'
import McpServersSection from './McpServersSection'
import RetentionSettingsSection from './RetentionSettingsSection'
import TlsSettingsSection from './TlsSettingsSection'
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
  | 'account'
  | 'appearance'
  | 'chat'
  | 'prompts'
  | 'workflows'
  | 'variables'
  | 'providers'
  | 'mcp'
  | 'plugins'
  | 'registry'
  | 'server'
  | 'security'
  | 'data'
  | 'tls'
  | 'users'

interface PageDef {
  id: SubPage
  title: string
  blurb: string
  adminOnly?: boolean
}

/**
 * Settings is a grouped two-pane layout: a sidebar of sub-pages under
 * group headers (persistent on desktop, hub list on mobile) and the
 * active sub-page's sections. Groups are purely visual — every page
 * keeps a stable id that doubles as the `/settings/<id>` URL segment
 * and the `settings-nav-<id>` test id.
 *
 * `adminOnly` sub-pages are hidden from non-admins because everything on
 * them mutates host-wide state (the Claude permission gate, the approved
 * command list, the global MCP server list, the binary itself). The
 * routes behind them are admin-gated server-side — this only keeps the
 * UI honest about what the API will accept.
 */
const GROUPS: { title: string | null; pages: PageDef[] }[] = [
  {
    title: null,
    pages: [
      { id: 'account', title: 'Account', blurb: 'Your user info, password and confirmations' },
    ],
  },
  {
    title: 'General',
    pages: [
      { id: 'appearance', title: 'Appearance', blurb: 'Theme, accent, text size, density, motion' },
      {
        id: 'chat',
        title: 'Chat & Models',
        blurb: 'Default model, caveman mode and the pre-hatcher model',
      },
    ],
  },
  {
    title: 'Agents',
    pages: [
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
        id: 'variables',
        title: 'Variables',
        blurb: 'Environment variables and shared agent variables',
      },
    ],
  },
  {
    title: 'Connections',
    pages: [
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
    ],
  },
  {
    title: 'Plugins',
    pages: [
      {
        id: 'plugins',
        title: 'Plugins',
        blurb: 'Installed plugins, approvals and their settings',
      },
      {
        id: 'registry',
        title: 'Plugin Registry',
        blurb: 'Browse and install plugins, manage registry repositories',
      },
    ],
  },
  {
    title: 'Administration',
    pages: [
      {
        id: 'server',
        title: 'Server',
        blurb: 'Ports, data directory, software updates',
        adminOnly: true,
      },
      {
        id: 'security',
        title: 'Security',
        blurb: 'Claude tool permissions and approved commands',
        adminOnly: true,
      },
      {
        id: 'data',
        title: 'Data',
        blurb: 'Backups and retention',
        adminOnly: true,
      },
      {
        id: 'tls',
        title: 'TLS / HTTPS',
        blurb: 'Certificate, hostnames, upload or regenerate',
        adminOnly: true,
      },
      {
        id: 'users',
        title: 'Users',
        blurb: 'Create and manage user accounts',
        adminOnly: true,
      },
    ],
  },
]

const SUB_PAGES: PageDef[] = GROUPS.flatMap((g) => g.pages)

/**
 * Search index: one row per section, addressed by the
 * `data-settings-anchor` wrapper it renders in. Sidebar search matches
 * page titles/blurbs plus these rows; a section hit jumps to its page
 * and scrolls the section into view with a brief highlight.
 */
const SECTION_INDEX: { page: SubPage; section: string; anchor: string; keywords: string }[] = [
  { page: 'account', section: 'User Info', anchor: 'user-info', keywords: 'username role' },
  { page: 'account', section: 'Password', anchor: 'password', keywords: 'change password' },
  {
    page: 'account',
    section: 'Confirmations',
    anchor: 'confirmations',
    keywords: 'backlog confirm warning ask again',
  },
  { page: 'appearance', section: 'Theme', anchor: 'theme', keywords: 'light dark auto' },
  { page: 'appearance', section: 'Accent Color', anchor: 'accent', keywords: 'hue swatch preset' },
  { page: 'appearance', section: 'Font Size', anchor: 'font-size', keywords: 'text scale' },
  {
    page: 'appearance',
    section: 'Density',
    anchor: 'density',
    keywords: 'compact comfortable spacing',
  },
  { page: 'appearance', section: 'Motion', anchor: 'motion', keywords: 'animation reduced' },
  {
    page: 'chat',
    section: 'Default Model',
    anchor: 'default-model',
    keywords: 'model effort routing new sessions',
  },
  { page: 'chat', section: 'Caveman Mode', anchor: 'caveman', keywords: 'terse output tokens' },
  {
    page: 'chat',
    section: 'Pre-hatcher Model',
    anchor: 'prehatch',
    keywords: 'research cheapest before message',
  },
  {
    page: 'prompts',
    section: 'System Prompts',
    anchor: 'prompts',
    keywords: 'library import auto-switch pre-hatcher prompt',
  },
  {
    page: 'workflows',
    section: 'Workflows',
    anchor: 'workflows',
    keywords: 'steps sequence custom cards projects',
  },
  {
    page: 'variables',
    section: 'Environment Variables',
    anchor: 'env-vars',
    keywords: 'env secrets encrypted lock injected',
  },
  {
    page: 'variables',
    section: 'Agent Variables',
    anchor: 'agent-vars',
    keywords: 'shared key value tools',
  },
  {
    page: 'providers',
    section: 'Providers',
    anchor: 'provider-visibility',
    keywords: 'hide toggle visibility model pickers',
  },
  {
    page: 'providers',
    section: 'Claude Accounts',
    anchor: 'claude-accounts',
    keywords: 'anthropic oauth login plan usage',
  },
  { page: 'providers', section: 'Grok Accounts', anchor: 'grok-accounts', keywords: 'xai login' },
  {
    page: 'providers',
    section: 'Kimi Accounts',
    anchor: 'kimi-accounts',
    keywords: 'moonshot login',
  },
  {
    page: 'providers',
    section: 'Ollama',
    anchor: 'ollama',
    keywords: 'local remote server pull model',
  },
  { page: 'providers', section: 'Cursor', anchor: 'cursor', keywords: 'cli binary discovery' },
  {
    page: 'providers',
    section: 'Provider Keep-Alive',
    anchor: 'keepalive',
    keywords: 'token stale ping login',
  },
  {
    page: 'mcp',
    section: 'MCP Servers',
    anchor: 'mcp',
    keywords: 'stdio http sse oauth tools external',
  },
  {
    page: 'plugins',
    section: 'Installed Plugins',
    anchor: 'plugins',
    keywords: 'wasm approve enable remove settings',
  },
  {
    page: 'registry',
    section: 'Plugin Registry',
    anchor: 'registry',
    keywords: 'browse install repositories templates',
  },
  {
    page: 'server',
    section: 'Server',
    anchor: 'server-info',
    keywords: 'port https data directory',
  },
  {
    page: 'server',
    section: 'Software Update',
    anchor: 'software-update',
    keywords: 'upgrade release restart version',
  },
  {
    page: 'security',
    section: 'Claude Tool Permissions',
    anchor: 'claude-permissions',
    keywords: 'bypass gate dangerously skip enforced',
  },
  {
    page: 'security',
    section: 'Approved Commands',
    anchor: 'approved-commands',
    keywords: 'always approve run command revoke',
  },
  { page: 'data', section: 'Backup', anchor: 'backup', keywords: 'download snapshot scheduled' },
  {
    page: 'data',
    section: 'Retention',
    anchor: 'retention',
    keywords: 'cleanup sweep age sessions events reports',
  },
  {
    page: 'tls',
    section: 'TLS / HTTPS',
    anchor: 'tls',
    keywords: 'certificate pem self-signed hostname',
  },
  {
    page: 'users',
    section: 'Users',
    anchor: 'users',
    keywords: 'accounts admin create delete password reset',
  },
]

/** 16×16 stroke icons for the section rail — one per sub-page, in the
 *  house inline-SVG style (see the app rail buttons). */
const NAV_ICON_PATHS: Record<SubPage, ReactNode> = {
  account: (
    <>
      <circle cx="8" cy="5.25" r="2.75" />
      <path d="M3 13.5a5 5 0 0 1 10 0" />
    </>
  ),
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
  variables: (
    <path d="M6 2.5c-1.5 0-2 .8-2 2v2c0 .8-.7 1.5-1.5 1.5.8 0 1.5.7 1.5 1.5v2c0 1.2.5 2 2 2M10 2.5c1.5 0 2 .8 2 2v2c0 .8.7 1.5 1.5 1.5-.8 0-1.5.7-1.5 1.5v2c0 1.2-.5 2-2 2" />
  ),
  providers: <path d="M6 2v3M10 2v3M4.5 5h7v2.5a3.5 3.5 0 0 1-7 0zM8 11v3" />,
  mcp: (
    <>
      <rect x="2.5" y="3" width="11" height="4" rx="1" />
      <rect x="2.5" y="9" width="11" height="4" rx="1" />
      <path d="M5 5h.01M5 11h.01" />
    </>
  ),
  plugins: (
    <>
      <rect x="2.5" y="2.5" width="11" height="11" rx="2" />
      <path d="M8 5.5v5M5.5 8h5" />
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
  security: (
    <>
      <path d="M8 2.5 13 4.5v3.5c0 3.1-2.1 5-5 5.5-2.9-.5-5-2.4-5-5.5V4.5z" />
      <path d="M6 8l1.5 1.5L10.5 6.5" />
    </>
  ),
  data: (
    <>
      <ellipse cx="8" cy="4" rx="5" ry="1.75" />
      <path d="M3 4v8c0 .97 2.24 1.75 5 1.75s5-.78 5-1.75V4M3 8c0 .97 2.24 1.75 5 1.75S13 8.97 13 8" />
    </>
  ),
  tls: (
    <>
      <rect x="3.5" y="7" width="9" height="6.5" rx="1.5" />
      <path d="M5.5 7V4.75a2.5 2.5 0 0 1 5 0V7M8 9.5v1.75" />
    </>
  ),
  users: (
    <>
      <circle cx="6" cy="5" r="2.5" />
      <path d="M2 13.5v-.75a4 4 0 0 1 8 0v.75M10.5 2.7a2.5 2.5 0 0 1 0 4.6M14 13.5v-.75a4 4 0 0 0-2.5-3.7" />
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
  /** Sub-page id to open on mount (from the `/settings/<id>` URL or a
   *  legacy deep link); unknown ids fall back to the default page. */
  initialSubPage?: string | null
}

export default function SettingsPage({ onBack, initialSubPage = null }: Props) {
  const user = useAuthStore((s) => s.user)
  const isAdmin = user?.role === 'admin'
  const visibleSubPages = SUB_PAGES.filter((p) => isAdmin || !p.adminOnly)
  // Desktop lands straight on Account — the sidebar is persistent there,
  // so an empty hub pane would be dead space. Mobile keeps the hub list
  // (`null`), which the nav rail renders as tappable cards.
  const [subPage, setSubPageRaw] = useState<SubPage | null>(() => {
    // Unknown ids AND admin-only ids out of this user's reach both fall
    // through to the default, so a bad deep link never shows a dead pane.
    const known = visibleSubPages.some((p) => p.id === initialSubPage)
    const initial = known ? (initialSubPage as SubPage) : null
    return initial ?? (window.matchMedia('(min-width: 769px)').matches ? 'account' : null)
  })
  const isMobile = useMediaQuery('(max-width: 768px)')
  // A non-admin must not land on — or stay on — an admin-only sub-page, even
  // through the `initialSubPage` deep link. Everything below renders off
  // `activeSubPage`, so an out-of-reach id falls back to the hub.
  const activeSubPage = visibleSubPages.some((p) => p.id === subPage) ? subPage : null

  /** Navigate to a sub-page, keeping the URL in sync (`/settings/<id>`)
   *  so refresh, back/forward and copied links restore the page. App.tsx
   *  re-parses the URL on popstate and remounts this component. */
  const setSubPage = (id: SubPage | null) => {
    setSubPageRaw(id)
    const path = id ? `/settings/${id}` : '/settings'
    if (window.location.pathname !== path) history.pushState(null, '', path)
  }

  // Sidebar search over pages and their sections.
  const [query, setQuery] = useState('')
  const [flashAnchor, setFlashAnchor] = useState<string | null>(null)
  // Account → Change Password opens the same self-mode modal as the user menu.
  const [showChangePassword, setShowChangePassword] = useState(false)

  useEffect(() => {
    // Legacy entry paths (`/plugins`, `/plugin-registry`, `/plugin-settings`,
    // `/users`) keep working as bookmarks but canonicalize to
    // `/settings/<id>` once mounted, so the address bar shows the real URL.
    const legacy = ['/plugins', '/plugin-registry', '/plugin-settings', '/users']
    if (legacy.includes(window.location.pathname) && activeSubPage) {
      history.replaceState(null, '', `/settings/${activeSubPage}`)
    }
    // Mount-only: canonicalization applies to the URL the page was opened on.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  useEffect(() => {
    if (!flashAnchor) return
    // The target page may not have committed yet when the jump comes from
    // a search hit — retry across a few frames until the anchor exists.
    let tries = 0
    let raf = 0
    const attempt = () => {
      const el = document.querySelector(`[data-settings-anchor="${flashAnchor}"]`)
      if (el) {
        el.scrollIntoView({ block: 'start' })
        el.classList.add('settings-section-flash')
      } else if (tries++ < 10) {
        raf = requestAnimationFrame(attempt)
      }
    }
    raf = requestAnimationFrame(attempt)
    const t = setTimeout(() => {
      document
        .querySelector(`[data-settings-anchor="${flashAnchor}"]`)
        ?.classList.remove('settings-section-flash')
      setFlashAnchor(null)
    }, 1600)
    return () => {
      cancelAnimationFrame(raf)
      clearTimeout(t)
    }
  }, [flashAnchor])

  const q = query.trim().toLowerCase()
  const sectionsFor = (page: SubPage) => SECTION_INDEX.filter((s) => s.page === page)
  const sectionMatches = (s: { section: string; keywords: string }) =>
    (s.section + ' ' + s.keywords).toLowerCase().includes(q)
  const pageMatches = (p: PageDef) =>
    q === '' ||
    p.title.toLowerCase().includes(q) ||
    p.blurb.toLowerCase().includes(q) ||
    sectionsFor(p.id).some(sectionMatches)
  const matchedSections = (p: PageDef) => (q === '' ? [] : sectionsFor(p.id).filter(sectionMatches))
  /** Groups filtered to what this user may see and what the query matches. */
  const visibleGroups = GROUPS.map((g) => ({
    title: g.title,
    pages: g.pages.filter((p) => (isAdmin || !p.adminOnly) && pageMatches(p)),
  })).filter((g) => g.pages.length > 0)
  /** A section hit jumps to the page, then scrolls + flashes the section. */
  const openSection = (page: SubPage, anchor: string) => {
    setSubPage(page)
    setFlashAnchor(anchor)
    setQuery('')
  }
  const openFirstMatch = () => {
    const first = visibleGroups[0]?.pages[0]
    if (!first) return
    const sections = matchedSections(first)
    if (sections.length > 0) openSection(sections[0].page, sections[0].anchor)
    else {
      setSubPage(first.id)
      setQuery('')
    }
  }
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
          onClick={() => (activeSubPage && isMobile ? setSubPage(null) : onBack())}
        >
          ← Back
        </button>
        <div className="settings-page-heading">
          <h2>{current ? `Settings · ${current.title}` : 'Settings'}</h2>
          {current && <p className="settings-page-blurb">{current.blurb}</p>}
        </div>
      </div>

      <div className="settings-content">
        {activeSubPage === 'account' && (
          <>
            <section className="settings-section" data-settings-anchor="user-info">
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

            <section className="settings-section" data-settings-anchor="password">
              <h3>Password</h3>
              <p className="form-hint">Change the password you sign in with.</p>
              <div className="settings-row">
                <button
                  type="button"
                  className="btn-secondary"
                  data-testid="account-change-password"
                  onClick={() => setShowChangePassword(true)}
                >
                  Change password
                </button>
              </div>
            </section>

            <section
              className="settings-section"
              data-testid="confirmations-section"
              data-settings-anchor="confirmations"
            >
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

        {activeSubPage === 'appearance' && (
          <>
            <section className="settings-section" data-settings-anchor="theme">
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

            <section
              className="settings-section"
              data-testid="accent-section"
              data-settings-anchor="accent"
            >
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

            <section
              className="settings-section"
              data-testid="font-size-section"
              data-settings-anchor="font-size"
            >
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

            <section
              className="settings-section"
              data-testid="density-section"
              data-settings-anchor="density"
            >
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

            <section
              className="settings-section"
              data-testid="motion-section"
              data-settings-anchor="motion"
            >
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
          </>
        )}

        {activeSubPage === 'chat' && (
          <>
            <section
              className="settings-section"
              data-testid="default-model-section"
              data-settings-anchor="default-model"
            >
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
                defaultLabel="None — route by effort"
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
            <section
              className="settings-section"
              data-testid="caveman-section"
              data-settings-anchor="caveman"
            >
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

            <section
              className="settings-section"
              data-testid="prehatch-section"
              data-settings-anchor="prehatch"
            >
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
            <section className="settings-section" data-settings-anchor="provider-visibility">
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
              <div data-settings-anchor="claude-accounts">
                <ClaudeAccountsSection />
              </div>
            )}

            {providerVisibility.find((p) => p.id === 'grok')?.hidden !== true && (
              <div data-settings-anchor="grok-accounts">
                <GrokAccountsSection />
              </div>
            )}

            {providerVisibility.find((p) => p.id === 'kimi')?.hidden !== true && (
              <div data-settings-anchor="kimi-accounts">
                <KimiAccountsSection />
              </div>
            )}

            {providerVisibility.find((p) => p.id === 'ollama')?.hidden !== true && (
              <section
                className="settings-section"
                data-testid="ollama-settings-section"
                data-settings-anchor="ollama"
              >
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
              <section
                className="settings-section"
                data-testid="cursor-settings-section"
                data-settings-anchor="cursor"
              >
                <h3>Cursor</h3>
                <p className="form-hint">
                  The cursor-agent CLI provider: binary path, default model, and model discovery.
                </p>
                <PluginSettingsForm pluginId="cursor" />
              </section>
            )}

            <section
              className="settings-section"
              data-testid="keepalive-section"
              data-settings-anchor="keepalive"
            >
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
            <section className="settings-section" data-settings-anchor="server-info">
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

            <div data-settings-anchor="software-update">
              <SoftwareUpdate />
            </div>
          </>
        )}

        {activeSubPage === 'security' && (
          <>
            <section
              className="settings-section"
              data-testid="claude-permissions-section"
              data-settings-anchor="claude-permissions"
            >
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

            <div data-settings-anchor="approved-commands">
              <ApprovedCommandsSection />
            </div>
          </>
        )}

        {activeSubPage === 'data' && (
          <>
            <section
              className="settings-section"
              data-testid="backup-section"
              data-settings-anchor="backup"
            >
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

            <div data-settings-anchor="retention">
              <RetentionSettingsSection />
            </div>
          </>
        )}

        {activeSubPage === 'tls' && (
          <div data-settings-anchor="tls">
            <TlsSettingsSection />
          </div>
        )}
        {activeSubPage === 'mcp' && (
          <div data-settings-anchor="mcp">
            <McpServersSection />
          </div>
        )}
        {activeSubPage === 'variables' && (
          <>
            <div data-settings-anchor="env-vars">
              <EnvVarsSection />
            </div>
            <div data-settings-anchor="agent-vars">
              <AgentVarsSection />
            </div>
          </>
        )}
        {activeSubPage === 'plugins' && (
          <div data-settings-anchor="plugins">
            <PluginsSection onBrowseRegistry={() => setSubPage('registry')} />
          </div>
        )}
        {activeSubPage === 'registry' && (
          <div data-settings-anchor="registry">
            <PluginRegistryPanel onManagePlugins={() => setSubPage('plugins')} />
          </div>
        )}
        {activeSubPage === 'prompts' && (
          <div data-settings-anchor="prompts">
            <SystemPromptsSection />
          </div>
        )}
        {activeSubPage === 'workflows' && (
          <div data-settings-anchor="workflows">
            <CustomWorkflowsSection />
          </div>
        )}
        {activeSubPage === 'users' && (
          <div data-settings-anchor="users">
            <UserManagement />
          </div>
        )}
      </div>

      <nav className="settings-nav" aria-label="Settings sections">
        <div className="settings-search">
          <input
            type="search"
            className="settings-search-input"
            placeholder="Search settings"
            aria-label="Search settings"
            data-testid="settings-search"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Escape') setQuery('')
              if (e.key === 'Enter') openFirstMatch()
            }}
          />
        </div>
        {visibleGroups.map((g) => (
          <div className="settings-nav-group" key={g.title ?? 'top'}>
            {g.title && <div className="settings-nav-group-title">{g.title}</div>}
            {g.pages.map((p) => (
              <div className="settings-nav-entry" key={p.id}>
                <button
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
                    {p.id === 'security' && claudeBypass && (
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
                {q !== '' &&
                  matchedSections(p).map((s) => (
                    <button
                      type="button"
                      className="settings-nav-subitem"
                      key={s.anchor}
                      data-testid={`settings-search-hit-${s.anchor}`}
                      onClick={() => openSection(s.page, s.anchor)}
                    >
                      {s.section}
                    </button>
                  ))}
              </div>
            ))}
          </div>
        ))}
        {q !== '' && visibleGroups.length === 0 && (
          <p className="settings-nav-empty" data-testid="settings-search-empty">
            No settings match &ldquo;{query}&rdquo;
          </p>
        )}
      </nav>

      {showChangePassword && (
        <ChangePasswordModal mode={{ kind: 'self' }} onClose={() => setShowChangePassword(false)} />
      )}
    </div>
  )
}
