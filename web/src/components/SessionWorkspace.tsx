import { useCallback, useEffect, useMemo, useState } from 'react'
import type { Event, Session } from '../types/api'
import { authedFetch } from '../store/auth'
import { useSessionsStore } from '../store/sessions'
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
/** Most panes on screen at once, the parent included. */
const MAX_PANES = 6
const MODE_KEY = 'peckboard.subagentPanes'

type PaneMode = 'auto' | 'off'

function readMode(): PaneMode {
  try {
    return localStorage.getItem(MODE_KEY) === 'off' ? 'off' : 'auto'
  } catch {
    return 'auto'
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

function viewportAspect(): number {
  return window.innerHeight > 0 ? window.innerWidth / window.innerHeight : 16 / 9
}

interface WorkspaceState {
  parentId: string
  /** Every child discovered so far, in discovery order. */
  known: string[]
  /** Children currently given a pane, in insertion order. */
  shown: string[]
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
  }, [sessionId])
  const fetchedList = useMemo(
    () => (fetched.parentId === sessionId ? fetched.list : []),
    [fetched, sessionId],
  )

  const sig = useSessionsStore((s) => subagentSig(s.eventsBySession[sessionId]))
  const refs = useMemo(() => JSON.parse(sig) as SubagentRef[], [sig])
  const nativeById = useMemo(() => {
    const m = new Map<string, Extract<SubagentRef, { kind: 'native' }>>()
    for (const r of refs) if (r.kind === 'native') m.set(refLeafId(r), r)
    return m
  }, [refs])
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

  const sessions = useSessionsStore((s) => s.sessions)
  const nameOf = (id: string): string =>
    sessions.find((s) => s.id === id)?.name ?? fetchedList.find((s) => s.id === id)?.name ?? ''

  const request = useSubagentPanesStore((s) => s.request)
  const [ws, setWs] = useState<WorkspaceState>(() => ({
    parentId: sessionId,
    known: [],
    shown: [],
    layout: leaf(PRIMARY),
    handledNonce: request?.nonce ?? 0,
  }))
  const [focusedKey, setFocusedKey] = useState<string | null>(null)

  /** Give `id` a pane: append while there's room, else swap it in for the
   *  focused child pane (or the newest one). */
  const showIn = useCallback(
    (state: WorkspaceState, id: string): WorkspaceState => {
      if (state.shown.includes(id)) return state
      if (state.shown.length < MAX_PANES - 1) return withShown(state, [...state.shown, id])
      const target =
        focusedKey && state.shown.includes(focusedKey)
          ? focusedKey
          : state.shown[state.shown.length - 1]
      const shown = state.shown.map((x) => (x === target ? id : x))
      return {
        ...state,
        shown,
        layout: state.layout ? replaceLeafSession(state.layout, target, id) : state.layout,
      }
    },
    [focusedKey],
  )

  // Reconcile discovery + "Show pane" requests while rendering (React's
  // adjust-state-on-prop-change pattern): no effect, no extra paint.
  let next = ws
  if (next.parentId !== sessionId) {
    next = {
      parentId: sessionId,
      known: [],
      shown: [],
      layout: leaf(PRIMARY),
      handledNonce: next.handledNonce,
    }
  }
  if (next.known.join('|') !== childIds.join('|')) {
    const fresh = childIds.filter((id) => !next.known.includes(id))
    next = { ...next, known: childIds }
    if (mode === 'auto') {
      for (const id of fresh) {
        if (next.shown.length < MAX_PANES - 1) next = withShown(next, [...next.shown, id])
      }
    }
  }
  if (request && request.nonce !== next.handledNonce) {
    next = { ...next, handledNonce: request.nonce }
    if (request.parentId === sessionId) next = showIn(next, request.leafId)
  }
  if (next !== ws) setWs(next)
  const state = next

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
          title="Open a pane for each subagent automatically"
          onClick={() => {
            const m: PaneMode = mode === 'auto' ? 'off' : 'auto'
            setMode(m)
            setWs(withShown(state, m === 'auto' ? state.known.slice(0, MAX_PANES - 1) : []))
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
        setWs((cur) =>
          withShown(
            cur,
            cur.shown.filter((x) => x !== entry.key),
          ),
        )
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
