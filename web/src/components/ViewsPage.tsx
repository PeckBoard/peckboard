import { useEffect, useMemo, useRef, useState, type FormEvent } from 'react'
import type { Session } from '../types/api'
import { useSessionsStore } from '../store/sessions'
import { useViewsStore, type ViewSummary } from '../store/views'
import {
  MAX_LEAVES,
  clearSession,
  countLeaves,
  insertAuto,
  replaceLeafSession,
  sessionIds,
  starterLayout,
  type LayoutNode,
  type LeafEntry,
  type StarterLayout,
} from '../lib/layoutTree'
import List from './List'
import ListViewHeader from './ListViewHeader'
import Modal from './Modal'
import ConfirmDialog from './ConfirmDialog'
import FieldError from './FieldError'
import RenameModal from './RenameModal'
import ChatView from './ChatView'
import SplitLayout, { type PaneInfo } from './SplitLayout'
import { MenuButton, type MenuItem } from './Dropdown'
import { SessionPaneStatus } from './panes'

const STARTERS: { value: StarterLayout; label: string }[] = [
  { value: 'columns', label: 'Columns' },
  { value: 'rows', label: 'Rows' },
  { value: 'grid', label: 'Grid' },
  { value: 'main-stack', label: 'Main + stack' },
]
const SAVE_DEBOUNCE_MS = 500
const EMPTY_VIEW_LAYOUT: LayoutNode = { kind: 'leaf', sessionId: null }

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
}

/** Top-level "Views" page: the saved-view list, or one view's split layout. */
export default function ViewsPage({
  activeViewId,
  onNavigate,
  getSessionMenuItems,
  onOpenSessionTab,
}: ViewsPageProps) {
  if (activeViewId) {
    return (
      <ViewEditor
        key={activeViewId}
        viewId={activeViewId}
        onBack={() => onNavigate(null)}
        getSessionMenuItems={getSessionMenuItems}
        onOpenSessionTab={onOpenSessionTab}
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

function NewViewModal({
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
  const [starter, setStarter] = useState<StarterLayout>('columns')
  const [touched, setTouched] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')

  const nameError = touched && !name.trim() ? 'Give the view a name' : ''
  const sessionsError = touched && picked.length === 0 ? 'Pick at least one session' : ''
  const full = picked.length >= MAX_LEAVES
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
      const v = await createView(name.trim(), starterLayout(starter, picked) ?? EMPTY_VIEW_LAYOUT)
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
              ({picked.length}/{MAX_LEAVES})
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
            onChange={(e) => setStarter(e.target.value as StarterLayout)}
            data-testid="new-view-starter"
          >
            {STARTERS.map((s) => (
              <option key={s.value} value={s.value}>
                {s.label}
              </option>
            ))}
          </select>
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

/** Searchable single-choice session picker (the "Add session" combobox). */
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

type SaveState = 'idle' | 'saving' | 'saved' | 'error'

function ViewEditor({
  viewId,
  onBack,
  getSessionMenuItems,
  onOpenSessionTab,
}: {
  viewId: string
  onBack: () => void
  getSessionMenuItems: (sessionId: string) => MenuItem[]
  onOpenSessionTab: (sessionId: string) => void
}) {
  const getView = useViewsStore((s) => s.getView)
  const updateView = useViewsStore((s) => s.updateView)
  const sessions = useChatSessions()
  const allSessions = useSessionsStore((s) => s.sessions)
  const [name, setName] = useState('')
  const [layout, setLayout] = useState<LayoutNode | null>(null)
  const [status, setStatus] = useState<'loading' | 'ready' | 'error'>('loading')
  const [loadError, setLoadError] = useState('')
  const [save, setSave] = useState<SaveState>('idle')
  const saveTimer = useRef<number | null>(null)
  const pending = useRef<LayoutNode | null | undefined>(undefined)

  useEffect(() => {
    let cancelled = false
    getView(viewId)
      .then((v) => {
        if (cancelled) return
        setName(v.name)
        // The API requires a layout; an empty view round-trips as one blank leaf.
        setLayout(v.layout?.kind === 'leaf' && v.layout.sessionId === null ? null : v.layout)
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
      updateView(viewId, { layout: next ?? EMPTY_VIEW_LAYOUT })
        .then(() => setSave(pending.current === undefined ? 'saved' : 'saving'))
        .catch(() => setSave('error'))
    }
  }, [viewId, updateView])
  // Leaving the page flushes a pending save rather than dropping it.
  useEffect(() => () => flush.current(), [])

  const change = (next: LayoutNode | null) => {
    setLayout(next)
    pending.current = next
    setSave('saving')
    if (saveTimer.current !== null) window.clearTimeout(saveTimer.current)
    saveTimer.current = window.setTimeout(() => flush.current(), SAVE_DEBOUNCE_MS)
  }
  const changeRef = useRef(change)
  useEffect(() => {
    changeRef.current = change
  })

  // A session deleted anywhere blanks its leaf into the "pick another" state.
  const layoutRef = useRef(layout)
  useEffect(() => {
    layoutRef.current = layout
  }, [layout])
  useEffect(() => {
    const onRemoved = (e: Event) => {
      const id = (e as CustomEvent<{ sessionId?: string }>).detail?.sessionId
      if (!id) return
      const cur = layoutRef.current
      const next = clearSession(cur, id)
      if (next !== cur) changeRef.current(next)
    }
    window.addEventListener('peckboard:session-removed', onRemoved)
    return () => window.removeEventListener('peckboard:session-removed', onRemoved)
  }, [])

  const inView = useMemo(() => new Set(sessionIds(layout)), [layout])
  const full = countLeaves(layout) >= MAX_LEAVES
  const nameOf = (id: string) => allSessions.find((s) => s.id === id)?.name ?? 'Session'
  const aspect = () => (window.innerHeight > 0 ? window.innerWidth / window.innerHeight : 16 / 9)

  const addSession = (id: string) => {
    if (inView.has(id) || full) return
    change(insertAuto(layout, id, aspect()))
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

  const getPaneInfo = (entry: LeafEntry): PaneInfo => {
    if (!entry.sessionId) return { title: 'Session deleted' }
    return {
      title: nameOf(entry.sessionId),
      statusSlot: <SessionPaneStatus sessionId={entry.sessionId} />,
      menuItems: getSessionMenuItems(entry.sessionId),
      canOpenAsTab: true,
    }
  }

  return (
    <div className="view-editor" data-testid="view-editor" data-view-id={viewId}>
      <ListViewHeader
        title={name}
        extras={
          <>
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
                  pending.current = layout
                  flush.current()
                }}
              >
                Retry
              </button>
            )}
            <SessionPicker
              sessions={sessions}
              exclude={inView}
              onPick={addSession}
              label="Add session"
              testId="view-add-session"
              disabled={full}
            />
            <button type="button" className="btn-secondary btn-sm" onClick={onBack}>
              All views
            </button>
          </>
        }
      />
      <SplitLayout
        layout={layout}
        onChange={change}
        rearrangeable
        testId="view-split-layout"
        getPaneInfo={getPaneInfo}
        onOpenAsTab={(entry) => entry.sessionId && onOpenSessionTab(entry.sessionId)}
        emptyState={
          <div className="split-empty-leaf" data-testid="view-empty">
            <p>This view has no panes.</p>
            <SessionPicker
              sessions={sessions}
              exclude={inView}
              onPick={addSession}
              label="Add session"
              testId="view-empty-add-session"
            />
          </div>
        }
        renderPane={(entry, ctx) =>
          entry.sessionId ? (
            <ChatView sessionId={entry.sessionId} compact shortcutsEnabled={ctx.focused} />
          ) : (
            <div className="split-empty-leaf" data-testid="view-deleted-leaf">
              <p>Session deleted — pick another</p>
              <SessionPicker
                sessions={sessions}
                exclude={inView}
                onPick={(id) => layout && change(replaceLeafSession(layout, entry.key, id))}
                label="Pick a session"
                testId="view-pick-session"
              />
            </div>
          )
        }
      />
    </div>
  )
}
