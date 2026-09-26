import { useCallback, useEffect, useMemo, useState } from 'react'
import type { Event, Session } from '../types/api'
import { authedFetch } from '../store/auth'
import { useSessionsStore } from '../store/sessions'
import { useWsStore } from '../store/ws'
import { nativeLeafId, useSubagentPanesStore } from '../store/subagentPanes'
import ChatView from './ChatView'
import SplitLayout, { type PaneInfo } from './SplitLayout'
import { MenuButton, type MenuItem } from './Dropdown'
import { NativeSubagentPane, PaneStatus, SessionPaneStatus } from './panes'
import { collectSubagents, type SubagentRef } from './chat/events'
import {
  leaf,
  reconcile,
  replaceLeafSession,
  type LayoutNode,
  type LeafEntry,
} from '../lib/layoutTree'

/** Leaf id of the parent session's own pane. Constant (not the session id)
 *  so switching sessions reuses the same pane and ChatView instance. */
const PRIMARY = '@primary'
/** Cap on panes a manual "Show pane" of a finished child may fill (the
 *  parent included); every running child always gets a pane in auto mode. */
const MAX_PANES = 6
const MODE_KEY = 'peckboard.subagentPanes'
/** Per-parent list of child panes the user closed: `{ [parentId]: leafId[] }`.
 *  Auto mode never reopens these, across remounts and reloads. */
const CLOSED_KEY = 'peckboard.subagentPanes.closed'
/** Parents remembered before the oldest entries are dropped. */
const MAX_CLOSED_PARENTS = 200

type PaneMode = 'auto' | 'off'

function readMode(): PaneMode {
  try {
    return localStorage.getItem(MODE_KEY) === 'off' ? 'off' : 'auto'
  } catch {
    return 'auto'
  }
}

function readClosedMap(): Record<string, string[]> {
  try {
    const raw: unknown = JSON.parse(localStorage.getItem(CLOSED_KEY) ?? '{}')
    return raw && typeof raw === 'object' && !Array.isArray(raw)
      ? (raw as Record<string, string[]>)
      : {}
  } catch {
    return {}
  }
}

function readClosed(parentId: string): string[] {
  const list = readClosedMap()[parentId]
  return Array.isArray(list) ? list.filter((x) => typeof x === 'string') : []
}

function writeClosed(parentId: string, closed: string[]) {
  try {
    const map = readClosedMap()
    delete map[parentId]
    if (closed.length > 0) map[parentId] = closed
    // Insertion order = recency; trim the oldest parents.
    const keys = Object.keys(map)
    for (const k of keys.slice(0, Math.max(0, keys.length - MAX_CLOSED_PARENTS))) delete map[k]
    localStorage.setItem(CLOSED_KEY, JSON.stringify(map))
  } catch {
    /* storage unavailable — closed state lives for this mount only */
  }
}

/** Subagent list per events array, cached by identity so the store selector
 *  below is O(1) for every event that isn't this session's. */
const subagentCache = new WeakMap<Event[], string>()
function subagentSig(events: Event[] | undefined): string {
  if (!events) return '[]'
  let sig = subagentCache.get(events)
  if (sig === undefined) {
    sig = JSON.stringify(collectSubagents(events))
    subagentCache.set(events, sig)
  }
  return sig
}

function refLeafId(r: SubagentRef): string {
  return r.kind === 'native' ? nativeLeafId(r.toolUseId) : r.sessionId
}

/** Running state from a session's live stream — the latest agent-start /
 *  agent-end — or null when the stream carries neither yet. Only a fallback
 *  for children the server has no parent link for (so no completion stamp). */
function liveRunning(events: Event[] | undefined): boolean | null {
  if (!events) return null
  for (let i = events.length - 1; i >= 0; i--) {
    if (events[i].kind === 'agent-start') return true
    if (events[i].kind === 'agent-end') return false
  }
  return null
}
function viewportAspect(): number {
  return window.innerHeight > 0 ? window.innerWidth / window.innerHeight : 16 / 9
}

interface WorkspaceState {
  parentId: string
  /** Every child discovered so far, in discovery order. */
  known: string[]
  /** Children currently given a pane, in insertion order. */
  shown: string[]
  /** Children the user closed; auto mode leaves these closed. */
  closed: string[]
  /** Children the user opened after they had finished; auto mode keeps
   *  these open (a pane opened while running closes with the child). */
  pinned: string[]
  /** Running children last reconciled against (`|`-joined). */
  runningSig: string
  layout: LayoutNode | null
  handledNonce: number
}

function withShown(ws: WorkspaceState, shown: string[]): WorkspaceState {
  return { ...ws, shown, layout: reconcile(ws.layout, [PRIMARY, ...shown], viewportAspect()) }
}

interface SessionWorkspaceProps {
  sessionId: string
  onOpenTodos?: () => void
  pluginItems?: { plugin: string; id: string; label: string }[]
  onOpenPlugin?: (itemId: string) => void
  /** Canonical session menu for a pane header (same list as the tab menu). */
  getSessionMenuItems: (sessionId: string) => MenuItem[]
  onOpenSessionTab: (sessionId: string) => void
}

/**
 * The normal session view: the session's ChatView, plus a split pane per
 * subagent it launches — Peckboard child sessions (spawn_subagent) and
 * Claude-native Agent/Task subagents. New children slide in via the
 * SplitLayout auto-tiler; with none (or the toggle Off) it's a bare ChatView.
 */
export default function SessionWorkspace({
  sessionId,
  onOpenTodos,
  pluginItems,
  onOpenPlugin,
  getSessionMenuItems,
  onOpenSessionTab,
}: SessionWorkspaceProps) {
  const [mode, setModeRaw] = useState<PaneMode>(readMode)
  const setMode = (m: PaneMode) => {
    setModeRaw(m)
    try {
      localStorage.setItem(MODE_KEY, m)
    } catch {
      /* storage unavailable — keep the in-memory choice */
    }
  }

  const sig = useSessionsStore((s) => subagentSig(s.eventsBySession[sessionId]))
  const refs = useMemo(() => JSON.parse(sig) as SubagentRef[], [sig])
  const nativeById = useMemo(() => {
    const m = new Map<string, Extract<SubagentRef, { kind: 'native' }>>()
    for (const r of refs) if (r.kind === 'native') m.set(refLeafId(r), r)
    return m
  }, [refs])
  // Refetch when the parent spawns another child, for its completion state.
  const spawnedCount = refs.filter((r) => r.kind === 'session').length
  // Peckboard child sessions known to the server (spawned before this open).
  const [fetched, setFetched] = useState<{ parentId: string; list: Session[] }>({
    parentId: sessionId,
    list: [],
  })
  useEffect(() => {
    let cancelled = false
    authedFetch(`/api/sessions/${sessionId}/children`)
      .then((res) => (res.ok ? res.json() : null))
      .then((data: unknown) => {
        if (cancelled || !data) return
        const raw = Array.isArray(data)
          ? data
          : ((data as { children?: unknown; sessions?: unknown }).children ??
            (data as { sessions?: unknown }).sessions)
        const list = Array.isArray(raw)
          ? (raw as Session[]).filter((s) => s && typeof s.id === 'string')
          : []
        setFetched({ parentId: sessionId, list })
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [sessionId, spawnedCount])
  // Completion lands live: the server broadcasts `session-updated` with the
  // child's row (incl. `subagent_completed_at`) when it marks it finished.
  useEffect(() => {
    const onUpdated = (e: CustomEvent<{ session_id: string; data: Session }>) => {
      const updated = e.detail?.data
      if (!updated || typeof updated !== 'object' || typeof updated.id !== 'string') return
      const parentOf = (updated as { parent_session_id?: unknown }).parent_session_id
      setFetched((prev) => {
        if (prev.parentId !== sessionId) return prev
        const i = prev.list.findIndex((s) => s.id === updated.id)
        if (i < 0) {
          return parentOf === sessionId ? { ...prev, list: [...prev.list, updated] } : prev
        }
        const list = prev.list.slice()
        list[i] = updated
        return { ...prev, list }
      })
    }
    window.addEventListener('peckboard:session-updated', onUpdated as EventListener)
    return () => {
      window.removeEventListener('peckboard:session-updated', onUpdated as EventListener)
    }
  }, [sessionId])
  const fetchedList = useMemo(
    () => (fetched.parentId === sessionId ? fetched.list : []),
    [fetched, sessionId],
  )

  const childIds = useMemo(() => {
    const ids: string[] = []
    const seen = new Set<string>()
    for (const id of [...fetchedList.map((s) => s.id), ...refs.map(refLeafId)]) {
      if (id === sessionId || seen.has(id)) continue
      seen.add(id)
      ids.push(id)
    }
    return ids
  }, [fetchedList, refs, sessionId])

  // Child sessions' `session-updated` frames only reach subscribers; keep
  // every child subscribed so completion lands without a pane open.
  const sessionChildSig = childIds.filter((id) => !nativeById.has(id)).join('|')
  useEffect(() => {
    if (!sessionChildSig) return
    const ids = sessionChildSig.split('|')
    const wsStore = useWsStore.getState()
    ids.forEach((id) => wsStore.subscribe(id))
    return () => ids.forEach((id) => wsStore.unsubscribe(id))
  }, [sessionChildSig])
  const liveSig = useWsStore((s) =>
    sessionChildSig
      ? sessionChildSig
          .split('|')
          .map((id) => {
            const r = liveRunning(s.eventsBySession[id])
            return r === null ? '-' : r ? '1' : '0'
          })
          .join('')
      : '',
  )
  const runningIds = useMemo(() => {
    const set = new Set<string>()
    for (const [id, r] of nativeById) if (r.running) set.add(id)
    const ids = sessionChildSig ? sessionChildSig.split('|') : []
    ids.forEach((id, i) => {
      // A child session is active until the server stamps its completion.
      // Its own agent-start / agent-end are no signal: it idles between
      // turns while waiting on background tasks. Only a child the server
      // doesn't list (just spawned, or no parent link) falls back to its
      // live stream.
      const row = fetchedList.find((s) => s.id === id)
      const running = row ? !row.subagent_completed_at : liveSig[i] !== '0'
      if (running) set.add(id)
    })
    return set
  }, [nativeById, sessionChildSig, liveSig, fetchedList])

  const sessions = useSessionsStore((s) => s.sessions)
  const nameOf = (id: string): string =>
    sessions.find((s) => s.id === id)?.name ?? fetchedList.find((s) => s.id === id)?.name ?? ''

  const request = useSubagentPanesStore((s) => s.request)
  const [ws, setWs] = useState<WorkspaceState>(() => ({
    parentId: sessionId,
    known: [],
    shown: [],
    closed: readClosed(sessionId),
    pinned: [],
    runningSig: '',
    layout: leaf(PRIMARY),
    handledNonce: request?.nonce ?? 0,
  }))
  const [focusedKey, setFocusedKey] = useState<string | null>(null)

  /** Give `id` a pane (reopening it if it was closed). A running child is
   *  appended outright — auto mode has no cap. A finished one is pinned so
   *  auto mode keeps it, appended while there's room, else swapped in for
   *  the focused finished pane (or the newest one). */
  const showIn = useCallback(
    (prev: WorkspaceState, id: string): WorkspaceState => {
      const base = prev.closed.includes(id)
        ? { ...prev, closed: prev.closed.filter((x) => x !== id) }
        : prev
      const running = runningIds.has(id)
      const state =
        running || base.pinned.includes(id) ? base : { ...base, pinned: [...base.pinned, id] }
      if (state.shown.includes(id)) return state
      if (running || state.shown.length < MAX_PANES - 1) {
        return withShown(state, [...state.shown, id])
      }
      const swappable = state.shown.filter((x) => !runningIds.has(x))
      if (swappable.length === 0) return withShown(state, [...state.shown, id])
      const target =
        focusedKey && swappable.includes(focusedKey) ? focusedKey : swappable[swappable.length - 1]
      const shown = state.shown.map((x) => (x === target ? id : x))
      return {
        ...state,
        shown,
        layout: state.layout ? replaceLeafSession(state.layout, target, id) : state.layout,
      }
    },
    [focusedKey, runningIds],
  )

  // Reconcile discovery + "Show pane" requests while rendering (React's
  // adjust-state-on-prop-change pattern): no effect, no extra paint.
  let next = ws
  if (next.parentId !== sessionId) {
    next = {
      parentId: sessionId,
      known: [],
      shown: [],
      closed: readClosed(sessionId),
      pinned: [],
      runningSig: '',
      layout: leaf(PRIMARY),
      handledNonce: next.handledNonce,
    }
  }
  // Auto mode shows every running child (plus any pinned after finishing):
  // new runners slide in, finished ones slide out. No cap.
  const runningList = childIds.filter((id) => runningIds.has(id))
  const runningSig = runningList.join('|')
  if (next.known.join('|') !== childIds.join('|') || next.runningSig !== runningSig) {
    next = { ...next, known: childIds, runningSig }
    if (mode === 'auto') {
      const { pinned, closed } = next
      const shown = next.shown.filter((id) => pinned.includes(id) || runningIds.has(id))
      for (const id of runningList) {
        if (closed.includes(id) || shown.includes(id)) continue
        shown.push(id)
      }
      if (shown.join('|') !== next.shown.join('|')) next = withShown(next, shown)
    }
  }
  if (request && request.nonce !== next.handledNonce) {
    next = { ...next, handledNonce: request.nonce }
    if (request.parentId === sessionId) next = showIn(next, request.leafId)
  }
  if (next !== ws) setWs(next)
  const state = next

  const closedSig = state.closed.join('|')
  useEffect(() => {
    writeClosed(state.parentId, closedSig ? closedSig.split('|') : [])
  }, [state.parentId, closedSig])

  // Tool cards only offer "Show pane" while this workspace is mounted.
  useEffect(() => {
    useSubagentPanesStore.getState().setWorkspace(sessionId)
    return () => useSubagentPanesStore.getState().setWorkspace(null)
  }, [sessionId])

  const overflow = state.known.filter((id) => !state.shown.includes(id))
  const childLabel = (id: string): string => {
    const n = nativeById.get(id)
    if (n) return n.description || n.subagentType || 'Sub-agent'
    return nameOf(id) || 'Subagent session'
  }

  const toolbarExtras =
    state.known.length > 0 ? (
      <>
        <button
          type="button"
          className="subagent-panes-toggle"
          aria-pressed={mode === 'auto'}
          data-testid="subagent-panes-toggle"
          data-mode={mode}
          title="Open a pane for each running subagent automatically"
          onClick={() => {
            const m: PaneMode = mode === 'auto' ? 'off' : 'auto'
            setMode(m)
            const open = state.known.filter(
              (id) => runningIds.has(id) && !state.closed.includes(id),
            )
            setWs(withShown({ ...state, pinned: [] }, m === 'auto' ? open : []))
          }}
        >
          Subagent panes: {mode === 'auto' ? 'Auto' : 'Off'}
        </button>
        {overflow.length > 0 && (
          <MenuButton
            ariaLabel={`${overflow.length} more subagents`}
            triggerClassName="subagent-overflow-chip"
            testId="subagent-overflow-chip"
            items={overflow.map((id) => ({
              label: childLabel(id),
              onSelect: () => setWs(showIn(state, id)),
              testId: 'subagent-overflow-item',
            }))}
          >
            +{overflow.length}
          </MenuButton>
        )}
      </>
    ) : null

  const getPaneInfo = (entry: LeafEntry): PaneInfo => {
    const id = entry.sessionId ?? ''
    if (id === PRIMARY) {
      return {
        title: nameOf(sessionId) || 'Session',
        statusSlot: <SessionPaneStatus sessionId={sessionId} />,
        menuItems: getSessionMenuItems(sessionId),
        closable: false,
      }
    }
    const native = nativeById.get(id)
    if (native) {
      return {
        title: `${native.subagentType ? `${native.subagentType}: ` : ''}${
          native.description || 'Sub-agent'
        }`,
        statusSlot: (
          <PaneStatus
            status={native.running ? 'working' : native.error ? 'error' : 'idle'}
            done={!native.running && !native.error}
          />
        ),
      }
    }
    return {
      title: nameOf(id) || 'Subagent session',
      statusSlot: <SessionPaneStatus sessionId={id} showDone />,
      menuItems: getSessionMenuItems(id),
      canOpenAsTab: true,
    }
  }

  return (
    <SplitLayout
      layout={state.layout}
      onChange={(layout) => setWs({ ...state, layout: layout ?? leaf(PRIMARY) })}
      onClosePane={(entry) => {
        setWs((cur) => ({
          ...withShown(
            cur,
            cur.shown.filter((x) => x !== entry.key),
          ),
          closed: cur.closed.includes(entry.key) ? cur.closed : [...cur.closed, entry.key],
          pinned: cur.pinned.filter((x) => x !== entry.key),
        }))
      }}
      onOpenAsTab={(entry) => entry.sessionId && onOpenSessionTab(entry.sessionId)}
      onFocusChange={setFocusedKey}
      bareWhenSingle
      testId="session-workspace"
      getPaneInfo={getPaneInfo}
      renderPane={(entry, ctx) => {
        const id = entry.sessionId ?? ''
        if (id === PRIMARY) {
          return (
            <ChatView
              sessionId={sessionId}
              onOpenTodos={onOpenTodos}
              pluginItems={pluginItems}
              onOpenPlugin={onOpenPlugin}
              compact={ctx.compact}
              shortcutsEnabled={ctx.focused}
              toolbarExtras={toolbarExtras}
            />
          )
        }
        const native = nativeById.get(id)
        if (native) {
          return (
            <NativeSubagentPane
              parentSessionId={sessionId}
              toolUseId={native.toolUseId}
              subagentType={native.subagentType}
              description={native.description}
              running={native.running}
            />
          )
        }
        return <ChatView sessionId={id} compact shortcutsEnabled={ctx.focused} />
      }}
    />
  )
}
