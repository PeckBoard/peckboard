import { useEffect, useMemo, useRef, useState, type FormEvent, type ReactNode } from 'react'
import type { Session } from '../types/api'
import { useSessionsStore } from '../store/sessions'
import { useProjectsStore } from '../store/projects'
import {
  patchWidget,
  terminalMeta,
  useViewsStore,
  widgetRef,
  withRef,
  type ViewProjectMeta,
  type ViewSummary,
  type ViewTerminalMeta,
  type ViewWidget,
  type WidgetPatch,
  type WidgetKind,
} from '../store/views'
import { authedFetch } from '../store/auth'
import {
  popOutTerminal,
  useTerminalsStore,
  type TerminalInfo,
  type TerminalPhase,
} from '../store/terminals'
import { useTabsStore } from '../store/tabs'
import {
  MAX_WIDGETS,
  compact,
  findFreeSlot,
  newWidgetId,
  starterRects,
  type StarterShape,
} from '../lib/widgetGrid'
import List from './List'
import ListViewHeader from './ListViewHeader'
import Modal from './Modal'
import ConfirmDialog from './ConfirmDialog'
import FieldError from './FieldError'
import RenameModal from './RenameModal'
import { MenuButton, type MenuItem } from './Dropdown'
import TerminalPicker from './terminal/TerminalPicker'
import { useTerminalPickerItems } from './terminal/useTerminalPickerItems'
import WidgetGrid, { type WidgetContext } from './dashboard/WidgetGrid'
import WidgetFrame from './dashboard/WidgetFrame'
import SessionWidget from './dashboard/SessionWidget'
import TerminalWidget from './dashboard/TerminalWidget'
import ProjectWidget, { ProjectPicker } from './dashboard/ProjectWidget'
import WidgetConfigModal from './dashboard/WidgetConfigModal'
import { WIDGET_CATEGORIES, WIDGET_SPECS, addTestId, isPane } from './dashboard/registry'
import type { WasmPlugin } from '../utils/pluginApproval'
import { describeActionError } from '../utils/actionError'
import '../styles/dashboard.css'

const STARTERS: { value: StarterShape; label: string }[] = [
  { value: 'columns', label: 'Columns' },
  { value: 'rows', label: 'Rows' },
  { value: 'grid', label: 'Grid' },
  { value: 'main-stack', label: 'Main + stack' },
]
const SAVE_DEBOUNCE_MS = 500

/** Chat sessions a view can show (experts never appear in chat lists). */
function useChatSessions(): Session[] {
  const sessions = useSessionsStore((s) => s.sessions)
  return useMemo(() => sessions.filter((s) => !s.is_expert), [sessions])
}

interface ViewsPageProps {
  activeViewId: string | null
  onNavigate: (id: string | null) => void
  getSessionMenuItems: (sessionId: string) => MenuItem[]
  onOpenSessionTab: (sessionId: string) => void
  onOpenTerminalTab: (terminalId: string) => void
  onOpenProject: (projectId: string) => void
  onOpenReport?: (folder: string, file: string) => void
}

/** Top-level "Views" page: the saved-view list, or one view's widget dashboard. */
export default function ViewsPage({
  activeViewId,
  onNavigate,
  getSessionMenuItems,
  onOpenSessionTab,
  onOpenTerminalTab,
  onOpenProject,
  onOpenReport,
}: ViewsPageProps) {
  if (activeViewId) {
    return (
      <ViewEditor
        key={activeViewId}
        viewId={activeViewId}
        onBack={() => onNavigate(null)}
        getSessionMenuItems={getSessionMenuItems}
        onOpenSessionTab={onOpenSessionTab}
        onOpenTerminalTab={onOpenTerminalTab}
        onOpenProject={onOpenProject}
        onOpenReport={onOpenReport}
      />
    )
  }
  return <ViewsList onOpen={(id) => onNavigate(id)} />
}

function formatUpdated(iso: string): string {
  const t = new Date(iso)
  return Number.isNaN(t.getTime()) ? '' : t.toLocaleString()
}

function ViewsList({ onOpen }: { onOpen: (id: string) => void }) {
  const views = useViewsStore((s) => s.views)
  const loaded = useViewsStore((s) => s.loaded)
  const error = useViewsStore((s) => s.error)
  const fetchViews = useViewsStore((s) => s.fetchViews)
  const renameView = useViewsStore((s) => s.updateView)
  const deleteView = useViewsStore((s) => s.deleteView)
  const [showNew, setShowNew] = useState(false)
  const [renaming, setRenaming] = useState<ViewSummary | null>(null)
  const [deleting, setDeleting] = useState<ViewSummary | null>(null)
  const [deleteBusy, setDeleteBusy] = useState(false)
  const [deleteError, setDeleteError] = useState<string | null>(null)

  useEffect(() => {
    void fetchViews()
  }, [fetchViews])

  return (
    <div className="list-view" data-testid="views-list">
      <ListViewHeader
        title="Views"
        actionLabel="+ New view"
        onAction={() => setShowNew(true)}
        actionTestId="views-new"
      />
      {error && (
        <div className="fetch-error-banner" role="alert">
          <span>{error}</span>
          <button type="button" onClick={() => void fetchViews()}>
            Retry
          </button>
        </div>
      )}
      <List
        items={views}
        getKey={(v) => v.id}
        onActivate={(v) => onOpen(v.id)}
        getMenuItems={(v) => [
          { label: 'Rename', onSelect: () => setRenaming(v) },
          { label: 'Delete', danger: true, onSelect: () => setDeleting(v) },
        ]}
        renderItem={(v) => (
          <>
            <span className="list-view-name" data-testid="view-list-row" data-view-id={v.id}>
              {v.name}
            </span>
            <span className="list-view-meta">
              <span className="list-view-time">{formatUpdated(v.updated_at)}</span>
            </span>
          </>
        )}
        emptyState={
          loaded ? (
            <div className="list-view-empty" data-testid="views-empty">
              <p>No views yet</p>
              <button className="list-view-empty-action" onClick={() => setShowNew(true)}>
                Create a view to watch several sessions side by side
              </button>
            </div>
          ) : (
            <div className="list-view-empty">
              <p>Loading views…</p>
            </div>
          )
        }
      />
      {showNew && (
        <NewViewModal
          onClose={() => setShowNew(false)}
          onCreated={(id) => {
            setShowNew(false)
            onOpen(id)
          }}
        />
      )}
      {renaming && (
        <RenameModal
          title="Rename view"
          label="View name"
          initialValue={renaming.name}
          onSubmit={async (name) => {
            await renameView(renaming.id, { name })
          }}
          onClose={() => setRenaming(null)}
        />
      )}
      {deleting && (
        <ConfirmDialog
          title="Delete view"
          message={`Delete “${deleting.name}”? The sessions in it are not affected.`}
          confirmLabel="Delete"
          danger
          busy={deleteBusy}
          error={deleteError}
          testId="view-delete-confirm"
          onConfirm={() => {
            setDeleteBusy(true)
            setDeleteError(null)
            deleteView(deleting.id)
              .then(() => setDeleting(null))
              .catch((e: unknown) =>
                setDeleteError(e instanceof Error ? e.message : 'Failed to delete view'),
              )
              .finally(() => setDeleteBusy(false))
          }}
          onCancel={() => {
            if (deleteBusy) return
            setDeleting(null)
            setDeleteError(null)
          }}
        />
      )}
    </div>
  )
}

/** New View dialog: name, sessions, starter layout. Also opened from the
 *  tab bar's `+ ▾` menu ("New split view"). */
export function NewViewModal({
  onClose,
  onCreated,
}: {
  onClose: () => void
  onCreated: (id: string) => void
}) {
  const createView = useViewsStore((s) => s.createView)
  const sessions = useChatSessions()
  const [name, setName] = useState('')
  const [picked, setPicked] = useState<string[]>([])
  const [search, setSearch] = useState('')
  const [starter, setStarter] = useState<StarterShape>('columns')
  const [touched, setTouched] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')

  const nameError = touched && !name.trim() ? 'Give the view a name' : ''
  const sessionsError = touched && picked.length === 0 ? 'Pick at least one session' : ''
  const full = picked.length >= MAX_WIDGETS
  const disabledReason = !name.trim()
    ? 'Enter a name'
    : picked.length === 0
      ? 'Pick at least one session'
      : ''

  const visible = useMemo(() => {
    const q = search.trim().toLowerCase()
    const sel = new Set(picked)
    return sessions
      .filter((s) => sel.has(s.id) || !q || s.name.toLowerCase().includes(q))
      .sort((a, b) => Number(sel.has(b.id)) - Number(sel.has(a.id)))
  }, [sessions, search, picked])

  const toggle = (id: string) =>
    setPicked((p) => (p.includes(id) ? p.filter((x) => x !== id) : [...p, id]))

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setTouched(true)
    if (disabledReason) return
    setBusy(true)
    setError('')
    try {
      const rects = starterRects(starter, picked.length)
      const widgets = picked.map((sessionId, i) =>
        withRef({ id: newWidgetId(), kind: 'session', ...rects[i] }, 'session', sessionId),
      )
      const v = await createView(name.trim(), widgets)
      onCreated(v.id)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create view')
      setBusy(false)
    }
  }

  return (
    <Modal onClose={onClose} maxWidth={560} data-testid="new-view-modal">
      <h2>New View</h2>
      <form onSubmit={submit}>
        <div className="form-field">
          <label className="form-label" htmlFor="new-view-name">
            Name
          </label>
          <input
            id="new-view-name"
            className="form-input"
            value={name}
            onChange={(e) => setName(e.target.value)}
            onBlur={() => setTouched(true)}
            placeholder="Release watch"
            maxLength={200}
            autoFocus
            data-testid="new-view-name"
          />
          <FieldError message={nameError} testId="new-view-name-error" />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="new-view-session-search">
            Sessions{' '}
            <span className="optional">
              ({picked.length}/{MAX_WIDGETS})
            </span>
          </label>
          <input
            id="new-view-session-search"
            className="form-input dependency-picker-search"
            type="search"
            placeholder="Search sessions…"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            role="combobox"
            aria-expanded="true"
            aria-controls="new-view-session-list"
            data-testid="new-view-session-search"
          />
          <div
            className="dependency-picker-list"
            id="new-view-session-list"
            role="listbox"
            aria-multiselectable="true"
            aria-label="Sessions"
          >
            {visible.length === 0 ? (
              <p className="form-hint dependency-picker-empty">
                {search.trim() ? 'No sessions match your search.' : 'No sessions yet.'}
              </p>
            ) : (
              visible.map((s) => {
                const checked = picked.includes(s.id)
                return (
                  <label
                    key={s.id}
                    className="dependency-picker-option"
                    data-testid="new-view-session-option"
                    data-session-id={s.id}
                  >
                    <input
                      type="checkbox"
                      checked={checked}
                      disabled={!checked && full}
                      onChange={() => toggle(s.id)}
                    />
                    <span className="dependency-picker-option-title">{s.name}</span>
                  </label>
                )
              })
            )}
          </div>
          <FieldError message={sessionsError} testId="new-view-sessions-error" />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="new-view-starter">
            Starter layout
          </label>
          <select
            id="new-view-starter"
            className="form-input"
            value={starter}
            onChange={(e) => setStarter(e.target.value as StarterShape)}
            data-testid="new-view-starter"
          >
            {STARTERS.map((s) => (
              <option key={s.value} value={s.value}>
                {s.label}
              </option>
            ))}
          </select>
          <p className="form-hint">
            A starting arrangement — drag and resize widgets freely afterwards.
          </p>
        </div>
        {error && (
          <p className="form-error" role="alert" data-testid="new-view-error">
            {error}
          </p>
        )}
        <div className="form-actions">
          {disabledReason && (
            <span className="form-actions-reason" data-testid="new-view-disabled-reason">
              {disabledReason}
            </span>
          )}
          <button type="button" className="btn-secondary" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={busy || !!disabledReason}
            data-testid="new-view-submit"
          >
            {busy ? 'Creating…' : 'Create view'}
          </button>
        </div>
      </form>
    </Modal>
  )
}

/** Searchable single-choice session picker. */
function SessionPicker({
  sessions,
  exclude,
  onPick,
  label,
  testId,
  disabled,
  className,
}: {
  sessions: Session[]
  exclude: Set<string>
  onPick: (id: string) => void
  label: string
  testId: string
  disabled?: boolean
  className?: string
}) {
  const items: MenuItem[] = sessions
    .filter((s) => !exclude.has(s.id))
    .map((s) => ({ label: s.name, searchText: s.id, onSelect: () => onPick(s.id) }))
  return (
    <MenuButton
      items={items}
      searchable
      searchPlaceholder="Search sessions…"
      searchTestId={`${testId}-search`}
      emptyLabel="No other sessions"
      listLabel="Sessions"
      haspopup="listbox"
      ariaLabel={label}
      triggerClassName={className ?? 'btn-secondary btn-sm'}
      testId={testId}
      disabled={disabled}
      minWidth={260}
    >
      {label}
    </MenuButton>
  )
}

/** Header "Add widget" menu, grouped by category (registry order). Panes
 *  and project-bound kinds open a searchable flyout of targets; scoped and
 *  global info widgets add immediately (scoped ones as "All projects"). */
function AddWidgetMenu({
  sessions,
  sessionsInView,
  projectsInView,
  disabled,
  onAdd,
  onTerminal,
}: {
  sessions: Session[]
  sessionsInView: Set<string>
  projectsInView: Set<string>
  disabled: boolean
  onAdd: (kind: WidgetKind, ref: string | null) => void
  onTerminal: (t: TerminalInfo) => void
}) {
  const projects = useProjectsStore((s) => s.projects)
  const projectsLoaded = useProjectsStore((s) => s.projectsLoaded)
  const fetchProjects = useProjectsStore((s) => s.fetchProjects)
  const term = useTerminalPickerItems('view-add-terminal', onTerminal)
  // Approved WASM plugins plus installed bundled crate plugins (listed in
  // `plugins` by id), for kinds that need one (ssh-fleet, github-bridge,
  // session-control).
  const [plugins, setPlugins] = useState<Set<string> | null>(null)
  const probePlugins = () => {
    authedFetch('/api/plugins')
      .then((res) => (res.ok ? res.json() : null))
      .then((body: { wasm_plugins?: WasmPlugin[]; plugins?: { id: string }[] } | null) =>
        setPlugins(
          new Set([
            ...(body?.wasm_plugins ?? []).filter((p) => p.status === 'approved').map((p) => p.name),
            ...(body?.plugins ?? []).map((p) => p.id),
          ]),
        ),
      )
      .catch(() => {})
  }

  const projectFlyout = (kind: WidgetKind, exclude?: Set<string>): MenuItem[] =>
    projects
      .filter((p) => !exclude?.has(p.id))
      .map((p) => ({
        label: p.name,
        hint: p.status === 'paused' ? 'paused' : undefined,
        searchText: p.id,
        testId: `${addTestId(kind)}-option-${p.id}`,
        onSelect: () => onAdd(kind, p.id),
      }))
  const itemFor = (kind: WidgetKind): MenuItem => {
    const spec = WIDGET_SPECS[kind]
    const base = { label: spec.label, hint: spec.hint, testId: addTestId(kind) }
    if (kind === 'session')
      return {
        ...base,
        searchable: true,
        searchPlaceholder: 'Search sessions…',
        searchTestId: 'view-add-session-search',
        emptyLabel: 'No other sessions',
        submenu: sessions
          .filter((s) => !sessionsInView.has(s.id))
          .map((s) => ({
            label: s.name,
            searchText: s.id,
            onSelect: () => onAdd('session', s.id),
          })),
      }
    if (kind === 'terminal')
      return {
        ...base,
        searchable: true,
        searchPlaceholder: 'Search terminals and hosts…',
        searchTestId: 'view-add-terminal-search',
        emptyLabel: 'No terminals or hosts',
        submenu: term.items,
      }
    if (spec.scope === 'required')
      return {
        ...base,
        searchable: true,
        searchPlaceholder: 'Search projects…',
        searchTestId: `${addTestId(kind)}-search`,
        emptyLabel: projectsLoaded ? 'No other projects' : 'Loading projects…',
        submenu: projectFlyout(kind, kind === 'project' ? projectsInView : undefined),
      }
    return { ...base, onSelect: () => onAdd(kind, null) }
  }
  const items: MenuItem[] = WIDGET_CATEGORIES.flatMap((cat) =>
    Object.values(WIDGET_SPECS)
      .filter((s) => s.category === cat.id && (!s.plugin || plugins?.has(s.plugin)))
      .map((s) => ({ ...itemFor(s.kind), group: { id: cat.id, label: cat.label } })),
  )
  return (
    <>
      <MenuButton
        items={items}
        ariaLabel="Add widget"
        triggerClassName="btn-primary btn-sm"
        testId="add-widget-button"
        disabled={disabled || term.busy}
        title={disabled ? `A view holds at most ${MAX_WIDGETS} widgets` : undefined}
        onOpen={() => {
          term.refresh()
          void fetchProjects()
          probePlugins()
        }}
      >
        {term.busy ? 'Opening…' : '+ Add widget'}
      </MenuButton>
      {term.error && (
        <span className="form-error" role="alert" data-testid="view-add-terminal-error">
          {term.error}
        </span>
      )}
    </>
  )
}

type SaveState = 'idle' | 'saving' | 'saved' | 'error'

function ViewEditor({
  viewId,
  onBack,
  getSessionMenuItems,
  onOpenSessionTab,
  onOpenTerminalTab,
  onOpenProject,
  onOpenReport,
}: {
  viewId: string
  onBack: () => void
  getSessionMenuItems: (sessionId: string) => MenuItem[]
  onOpenSessionTab: (sessionId: string) => void
  onOpenTerminalTab: (terminalId: string) => void
  onOpenProject: (projectId: string) => void
  onOpenReport?: (folder: string, file: string) => void
}) {
  const getView = useViewsStore((s) => s.getView)
  const updateView = useViewsStore((s) => s.updateView)
  const createTerminal = useTerminalsStore((s) => s.create)
  const closeTerminal = useTerminalsStore((s) => s.close)
  const sessions = useChatSessions()
  const projects = useProjectsStore((s) => s.projects)
  const allSessions = useSessionsStore((s) => s.sessions)
  const [name, setName] = useState('')
  const [widgets, setWidgets] = useState<ViewWidget[]>([])
  const [status, setStatus] = useState<'loading' | 'ready' | 'error'>('loading')
  const [loadError, setLoadError] = useState('')
  const [save, setSave] = useState<SaveState>('idle')
  const [termMeta, setTermMeta] = useState<Record<string, ViewTerminalMeta>>({})
  const [projMeta, setProjMeta] = useState<Record<string, ViewProjectMeta>>({})
  const [termPhase, setTermPhase] = useState<Record<string, TerminalPhase>>({})
  const [changingId, setChangingId] = useState<string | null>(null)
  const [configuringId, setConfiguringId] = useState<string | null>(null)
  const [cardMeta, setCardMeta] = useState<Record<string, { title: string }>>({})
  const [closingTerminal, setClosingTerminal] = useState<string | null>(null)
  const [closeBusy, setCloseBusy] = useState(false)
  const [closeError, setCloseError] = useState<string | null>(null)
  const saveTimer = useRef<number | null>(null)
  const pending = useRef<ViewWidget[] | undefined>(undefined)

  useEffect(() => {
    let cancelled = false
    getView(viewId)
      .then((v) => {
        if (cancelled) return
        setName(v.name)
        setTermMeta(v.terminals ?? {})
        setProjMeta(v.projects ?? {})
        setCardMeta(v.cards ?? {})
        setWidgets(compact(v.widgets))
        setStatus('ready')
      })
      .catch((e: unknown) => {
        if (cancelled) return
        setLoadError(e instanceof Error ? e.message : 'Failed to load view')
        setStatus('error')
      })
    return () => {
      cancelled = true
    }
  }, [viewId, getView])

  const flush = useRef<() => void>(() => {})
  useEffect(() => {
    flush.current = () => {
      if (saveTimer.current !== null) {
        window.clearTimeout(saveTimer.current)
        saveTimer.current = null
      }
      const next = pending.current
      if (next === undefined) return
      pending.current = undefined
      setSave('saving')
      updateView(viewId, { widgets: next })
        .then((v) => {
          if (v.projects) setProjMeta((m) => ({ ...m, ...v.projects }))
          if (v.cards) setCardMeta((m) => ({ ...m, ...v.cards }))
          setSave(pending.current === undefined ? 'saved' : 'saving')
        })
        .catch(() => setSave('error'))
    }
  }, [viewId, updateView])
  // Leaving the page flushes a pending save rather than dropping it.
  useEffect(() => () => flush.current(), [])

  const change = (next: ViewWidget[]) => {
    setWidgets(next)
    pending.current = next
    setSave('saving')
    if (saveTimer.current !== null) window.clearTimeout(saveTimer.current)
    saveTimer.current = window.setTimeout(() => flush.current(), SAVE_DEBOUNCE_MS)
  }
  const changeRef = useRef(change)
  useEffect(() => {
    changeRef.current = change
  })
  const widgetsRef = useRef(widgets)
  useEffect(() => {
    widgetsRef.current = widgets
  }, [widgets])

  // A session deleted anywhere blanks its widget into the "pick another" state.
  useEffect(() => {
    const onRemoved = (e: Event) => {
      const id = (e as CustomEvent<{ sessionId?: string }>).detail?.sessionId
      if (!id) return
      const cur = widgetsRef.current
      if (!cur.some((w) => w.kind === 'session' && w.sessionId === id)) return
      changeRef.current(
        cur.map((w) =>
          w.kind === 'session' && w.sessionId === id ? withRef(w, 'session', null) : w,
        ),
      )
    }
    window.addEventListener('peckboard:session-removed', onRemoved)
    return () => window.removeEventListener('peckboard:session-removed', onRemoved)
  }, [])

  const refsOf = (kind: WidgetKind) =>
    new Set(
      widgets
        .filter((w) => w.kind === kind)
        .map(widgetRef)
        .filter((r): r is string => !!r),
    )
  const sessionsInView = refsOf('session')
  const projectsInView = refsOf('project')
  const full = widgets.length >= MAX_WIDGETS
  const nameOf = (id: string) => allSessions.find((s) => s.id === id)?.name ?? 'Session'

  const rememberTerminal = (t: TerminalInfo) =>
    setTermMeta((m) => ({ ...m, [t.id]: terminalMeta(t) }))
  const markClosed = (id: string) =>
    setTermMeta((m) => (m[id] && !m[id].closed ? { ...m, [id]: { ...m[id], closed: true } } : m))
  const rememberProject = (id: string) => {
    const p = useProjectsStore.getState().projects.find((x) => x.id === id)
    if (p) setProjMeta((m) => ({ ...m, [id]: { name: p.name } }))
  }

  const addWidget = (kind: WidgetKind, ref: string | null) => {
    const cur = widgetsRef.current
    if (cur.length >= MAX_WIDGETS) return
    if (WIDGET_SPECS[kind].scope !== 'none' && ref) rememberProject(ref)
    const size = WIDGET_SPECS[kind].size
    const base = compact(cur)
    const slot = findFreeSlot(base, size.w, size.h)
    change(compact([...base, withRef({ id: newWidgetId(), kind, ...slot }, kind, ref)]))
  }
  const addTerminal = (t: TerminalInfo) => {
    rememberTerminal(t)
    addWidget('terminal', t.id)
  }
  const setRef = (widgetId: string, kind: WidgetKind, ref: string) => {
    if (WIDGET_SPECS[kind].scope !== 'none') rememberProject(ref)
    change(widgetsRef.current.map((w) => (w.id === widgetId ? withRef(w, kind, ref) : w)))
  }
  /** Persist widget-owned fields (scope, note body, pinned report, …). */
  const patchById = (widgetId: string, patch: WidgetPatch) => {
    if (patch.projectId) rememberProject(patch.projectId)
    change(widgetsRef.current.map((w) => (w.id === widgetId ? patchWidget(w, patch) : w)))
  }
  const removeWidget = (widgetId: string) =>
    change(compact(widgetsRef.current.filter((w) => w.id !== widgetId)))
  /** Open a fresh shell on a closed terminal's host and point every widget
   *  that showed the old one at it (mirrors stay mirrors). */
  const reopenTerminal = async (oldId: string) => {
    const meta = termMeta[oldId]
    if (!meta) return
    const t = await createTerminal(meta.plugin_id, meta.host_id)
    rememberTerminal(t)
    change(
      widgetsRef.current.map((w) =>
        w.kind === 'terminal' && w.terminalId === oldId ? withRef(w, 'terminal', t.id) : w,
      ),
    )
  }
  const confirmCloseTerminal = () => {
    const id = closingTerminal
    if (!id) return
    setCloseBusy(true)
    setCloseError(null)
    closeTerminal(id)
      .then(() => {
        markClosed(id)
        useTabsStore.getState().removeTabsForItem('terminal', id)
        setClosingTerminal(null)
      })
      .catch((e: unknown) => setCloseError(describeActionError(e, "Couldn't close the terminal.")))
      .finally(() => setCloseBusy(false))
  }

  if (status === 'loading') {
    return (
      <div className="list-view">
        <div className="list-view-empty">
          <p>Loading view…</p>
        </div>
      </div>
    )
  }
  if (status === 'error') {
    return (
      <div className="list-view">
        <div className="fetch-error-pane" role="alert" data-testid="view-load-error">
          <p>{loadError}</p>
          <button type="button" onClick={onBack}>
            Back to views
          </button>
        </div>
      </div>
    )
  }

  /** Session / terminal / project pickers that point widget `widgetId` at
   *  a new target (switching its kind if needed). */
  const fillPickers = (widgetId: string, testPrefix: string, after?: () => void) => (
    <>
      <SessionPicker
        sessions={sessions}
        exclude={sessionsInView}
        onPick={(id) => {
          setRef(widgetId, 'session', id)
          after?.()
        }}
        label="Session…"
        testId={`${testPrefix}-session`}
      />
      <TerminalPicker
        onPick={(t) => {
          rememberTerminal(t)
          setRef(widgetId, 'terminal', t.id)
          after?.()
        }}
        label="Terminal…"
        testId={`${testPrefix}-terminal`}
      />
      <ProjectPicker
        exclude={projectsInView}
        onPick={(id) => {
          setRef(widgetId, 'project', id)
          after?.()
        }}
        label="Project…"
        testId={`${testPrefix}-project`}
      />
    </>
  )

  const changeItem: (w: ViewWidget) => MenuItem = (w) => ({
    label: 'Change…',
    testId: 'widget-change',
    onSelect: () => setChangingId(w.id),
  })
  const removeItem: (w: ViewWidget) => MenuItem = (w) => ({
    label: 'Remove',
    testId: 'widget-remove',
    onSelect: () => removeWidget(w.id),
  })

  const configureItem: (w: ViewWidget) => MenuItem = (w) => ({
    label: 'Configure…',
    testId: 'widget-configure',
    onSelect: () => setConfiguringId(w.id),
  })
  const openHostTerminal = (pluginId: string, hostId: string) => {
    createTerminal(pluginId, hostId)
      .then((t) => onOpenTerminalTab(t.id))
      .catch(() => {})
  }

  /** Info widgets render their registry component with the page's standard
   *  menu; a project-bound kind without a project shows a picker instead. */
  const renderInfo = (w: ViewWidget, ctx: WidgetContext): ReactNode => {
    const spec = WIDGET_SPECS[w.kind]
    const Info = spec.component
    const pid = spec.scope === 'none' ? null : (w.projectId ?? null)
    if (!Info || (spec.scope === 'required' && !pid)) {
      return (
        <WidgetFrame
          kind={w.kind}
          widgetId={w.id}
          title={spec.label}
          menuItems={[removeItem(w)]}
          ctx={ctx}
        >
          <div className="split-empty-leaf" data-testid="view-scope-leaf">
            <p>Pick a project to show its {spec.label.toLowerCase()}</p>
            <div className="view-terminal-closed-actions">
              <ProjectPicker
                onPick={(id) => patchById(w.id, { projectId: id })}
                label="Project…"
                testId="view-pick-scope"
              />
            </div>
          </div>
        </WidgetFrame>
      )
    }
    return (
      <Info
        widget={w}
        ctx={ctx}
        menuItems={[
          ...(spec.configurable ? [configureItem(w), { divider: true }] : []),
          removeItem(w),
        ]}
        scopeProjectId={pid}
        scopeName={
          pid ? (projMeta[pid]?.name ?? projects.find((p) => p.id === pid)?.name) : undefined
        }
        onOpenSession={onOpenSessionTab}
        onOpenProject={onOpenProject}
        onChange={(patch) => patchById(w.id, patch)}
        onOpenTerminal={openHostTerminal}
        onOpenReport={onOpenReport}
      />
    )
  }

  const renderWidget = (w: ViewWidget, ctx: WidgetContext): ReactNode => {
    if (!isPane(w.kind)) return renderInfo(w, ctx)
    const ref = widgetRef(w)
    if (!ref) {
      const what = w.kind === 'session' ? 'session' : w.kind === 'terminal' ? 'terminal' : 'project'
      return (
        <WidgetFrame
          kind={w.kind}
          widgetId={w.id}
          title="Empty widget"
          menuItems={[removeItem(w)]}
          ctx={ctx}
        >
          <div className="split-empty-leaf" data-testid="view-deleted-leaf">
            <p>Nothing to show — pick a {what}, or switch this widget to something else</p>
            <div className="view-terminal-closed-actions">{fillPickers(w.id, 'view-pick')}</div>
          </div>
        </WidgetFrame>
      )
    }
    if (w.kind === 'session') {
      const own = getSessionMenuItems(ref)
      return (
        <SessionWidget
          widgetId={w.id}
          sessionId={ref}
          title={nameOf(ref)}
          ctx={ctx}
          menuItems={[
            {
              label: 'Open in tab',
              testId: 'widget-open-tab',
              onSelect: () => onOpenSessionTab(ref),
            },
            changeItem(w),
            ...(own.length > 0 ? [{ divider: true }, ...own] : []),
            { divider: true },
            removeItem(w),
          ]}
        />
      )
    }
    if (w.kind === 'terminal') {
      const meta = termMeta[ref]
      const closed = !!meta?.closed
      return (
        <TerminalWidget
          widgetId={w.id}
          terminalId={ref}
          meta={meta}
          phase={termPhase[w.id] ?? 'connecting'}
          ctx={ctx}
          menuItems={[
            {
              label: 'Open in tab',
              testId: 'widget-open-tab',
              hidden: closed,
              onSelect: () => onOpenTerminalTab(ref),
            },
            { label: 'Pop out', onSelect: () => popOutTerminal(ref), hidden: closed },
            changeItem(w),
            { divider: true },
            {
              label: 'Close terminal',
              danger: true,
              hidden: closed,
              testId: 'view-terminal-close',
              onSelect: () => setClosingTerminal(ref),
            },
            removeItem(w),
          ]}
          onStatus={(s) => {
            setTermPhase((p) => (p[w.id] === s.phase ? p : { ...p, [w.id]: s.phase }))
            // Closed elsewhere (another tab, the Terminals page).
            if (s.phase === 'ended' && s.message === 'Terminal closed') markClosed(ref)
          }}
          onReopen={() => reopenTerminal(ref)}
          replaceSlot={
            <button
              type="button"
              className="btn-secondary btn-sm"
              data-testid="view-terminal-replace"
              onClick={() => setChangingId(w.id)}
            >
              Change…
            </button>
          }
        />
      )
    }
    return (
      <ProjectWidget
        widgetId={w.id}
        projectId={ref}
        fallbackName={projMeta[ref]?.name}
        ctx={ctx}
        onOpenProject={onOpenProject}
        onOpenSession={onOpenSessionTab}
        pickerSlot={fillPickers(w.id, 'view-pick')}
        menuItems={[
          { label: 'Open in tab', testId: 'widget-open-tab', onSelect: () => onOpenProject(ref) },
          changeItem(w),
          { divider: true },
          removeItem(w),
        ]}
      />
    )
  }

  return (
    <div className="view-editor" data-testid="view-editor" data-view-id={viewId}>
      <ListViewHeader
        title={name}
        extras={
          <div className="view-editor-actions">
            <span
              className="view-save-state"
              data-testid="view-save-state"
              data-state={save}
              aria-live="polite"
            >
              {save === 'saving'
                ? 'Saving…'
                : save === 'saved'
                  ? 'Saved'
                  : save === 'error'
                    ? 'Save failed'
                    : ''}
            </span>
            {save === 'error' && (
              <button
                type="button"
                className="btn-secondary btn-sm"
                onClick={() => {
                  pending.current = widgets
                  flush.current()
                }}
              >
                Retry
              </button>
            )}
            <AddWidgetMenu
              sessions={sessions}
              sessionsInView={sessionsInView}
              projectsInView={projectsInView}
              disabled={full}
              onAdd={addWidget}
              onTerminal={addTerminal}
            />
            <button type="button" className="btn-secondary btn-sm" onClick={onBack}>
              All views
            </button>
          </div>
        }
      />
      <div className="view-dashboard">
        <WidgetGrid
          items={widgets}
          onChange={change}
          renderWidget={renderWidget}
          testId="view-widget-grid"
          emptyState={
            <div className="widget-grid-empty-state" data-testid="view-empty">
              <p className="widget-grid-empty-title">This view has no widgets yet</p>
              <p className="form-hint">
                Add sessions, terminals, or project summaries, then drag and resize them into place.
              </p>
              <div className="view-terminal-closed-actions">
                <SessionPicker
                  sessions={sessions}
                  exclude={sessionsInView}
                  onPick={(id) => addWidget('session', id)}
                  label="Add session"
                  testId="view-empty-add-session"
                />
                <TerminalPicker
                  onPick={addTerminal}
                  label="Add terminal"
                  testId="view-empty-add-terminal"
                />
                <ProjectPicker
                  exclude={projectsInView}
                  onPick={(id) => addWidget('project', id)}
                  label="Add project"
                  testId="view-empty-add-project"
                />
              </div>
            </div>
          }
        />
      </div>
      {changingId && (
        <Modal onClose={() => setChangingId(null)} maxWidth={460} data-testid="view-replace-modal">
          <h2>Change Widget</h2>
          <p className="form-hint">
            Show a session, a terminal, or a project summary here instead. Position and size stay.
          </p>
          <div className="form-actions">
            {fillPickers(changingId, 'view-replace', () => setChangingId(null))}
            <button type="button" className="btn-secondary" onClick={() => setChangingId(null)}>
              Cancel
            </button>
          </div>
        </Modal>
      )}
      {(() => {
        const cw = configuringId ? widgets.find((w) => w.id === configuringId) : undefined
        return cw ? (
          <WidgetConfigModal
            widget={cw}
            cardTitle={cw.cardId ? cardMeta[cw.cardId]?.title : undefined}
            onPatch={(patch) => patchById(cw.id, patch)}
            onClose={() => setConfiguringId(null)}
          />
        ) : null
      })()}
      {closingTerminal && (
        <ConfirmDialog
          title="Close terminal"
          message={`Close “${termMeta[closingTerminal]?.name ?? 'this terminal'}”? The remote shell is ended and anything running in it stops. Widgets showing it offer to reopen one on the same host.`}
          confirmLabel="Close"
          cancelLabel="Cancel"
          danger
          busy={closeBusy}
          error={closeError}
          testId="view-terminal-close-confirm"
          onConfirm={confirmCloseTerminal}
          onCancel={() => {
            if (closeBusy) return
            setClosingTerminal(null)
            setCloseError(null)
          }}
        />
      )}
    </div>
  )
}
