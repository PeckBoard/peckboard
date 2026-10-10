import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react'
import WidgetFrame from '../WidgetFrame'
import { DashEmpty, DashError, DashLoading, DashNoMatch } from './DashParts'
import {
  FilterBar,
  FilterButton,
  inSet,
  matchesSearch,
  useWidgetFilters,
  type FilterDef,
} from './filters'
import { humanize, relativeTime } from './useDashboardData'
import { pluginUiFetch, pluginUiPost, PluginUiError } from './pluginUi'
import type { InfoWidgetProps } from './types'
import '../../../styles/dashboard-info.css'
import '../../../styles/dashboard-plugins.css'

const PLUGIN = 'session-control'
const POLL_MS = 15_000
/** Activity events shown when a row is expanded. */
const RECENT_EVENTS = 5

/** `GoalStatus` in `peck-plugins/session-control/src/state.rs`. */
interface GoalStatus {
  state: string // in_progress | blocked | done
  note: string
  percent: number | null
  updated_at: string
  eta: { minutes_remaining: number; projected_at: string; updated_at: string } | null
}

interface ActivityEvent {
  ts: string
  kind: string
  detail: string
}

/** The subset of `Orchestrator` (+ `brain_busy` from `with_liveness`) the
 *  widget shows. */
interface Orchestrator {
  id: string
  name: string
  enabled: boolean
  paused: boolean
  goal: string
  goal_status: GoalStatus
  session_id: string | null
  schedule: { every_minutes: number | null }
  stats: { last_fired_at: string | null; next_due_at: string | null; fires: number }
  log: ActivityEvent[]
  error: string | null
  backoff_until: string | null
  brain_busy: boolean
}

interface OrchList {
  orchestrators: Orchestrator[]
  global_paused: boolean
  clock: string
}

const GOAL_TONE: Record<string, string> = {
  in_progress: 'run',
  blocked: 'warn',
  done: 'ok',
}

type Tone = 'running' | 'idle' | 'paused' | 'error'

function stateOf(o: Orchestrator, globalPaused: boolean): { tone: Tone; label: string } {
  if (o.brain_busy) return { tone: 'running', label: 'Running' }
  if (!o.enabled) return { tone: 'paused', label: o.error ? 'Disabled after errors' : 'Disabled' }
  if (o.paused || globalPaused) return { tone: 'paused', label: 'Paused' }
  if (o.error) return { tone: 'error', label: `Error: ${o.error}` }
  return { tone: 'idle', label: 'Idle' }
}
/** A log timestamp, or null for the 1970 placeholder the plugin stamps
 *  before its engine clock's first tick (and for blanks). */
function logTs(ts: string): string | null {
  const t = Date.parse(ts)
  return Number.isFinite(t) && t > Date.UTC(2000, 0, 1) ? ts : null
}

function useOrchestrators() {
  const [data, setData] = useState<OrchList | null>(null)
  const [error, setError] = useState<{ message: string; notInstalled: boolean } | null>(null)
  const seq = useRef(0)

  const load = useCallback(async () => {
    const mine = ++seq.current
    try {
      const d = await pluginUiFetch<OrchList>(PLUGIN, '/orchestrators')
      if (mine !== seq.current) return
      setData(d)
      setError(null)
    } catch (e) {
      if (mine !== seq.current) return
      setError({
        message: e instanceof Error ? e.message : 'Failed to load',
        notInstalled: e instanceof PluginUiError && e.notInstalled,
      })
    }
  }, [])

  useEffect(() => {
    const first = window.setTimeout(() => void load(), 0)
    const t = window.setInterval(() => {
      if (document.visibilityState === 'visible') void load()
    }, POLL_MS)
    return () => {
      window.clearTimeout(first)
      window.clearInterval(t)
    }
  }, [load])

  return { data, error, reload: load }
}

const STATE_OPTIONS = [
  { value: 'running', label: 'Running' },
  { value: 'paused', label: 'Paused' },
  { value: 'disabled', label: 'Disabled' },
]

/** Bucket for the state chips, in `stateOf`'s precedence. */
function stateKey(o: Orchestrator, globalPaused: boolean): string {
  if (o.brain_busy) return 'running'
  if (!o.enabled) return 'disabled'
  if (o.paused || globalPaused) return 'paused'
  return o.error ? 'error' : 'idle'
}

/** Orchestrator goals at a glance: state, goal + status, next / last run,
 *  latest activity, with Run now and Pause / Resume. A row expands to its
 *  recent activity. */
export default function OrchestratorsWidget(props: InfoWidgetProps) {
  const { widget, ctx, menuItems, onOpenSession } = props
  const { data, error, reload } = useOrchestrators()
  const [expanded, setExpanded] = useState<string | null>(null)
  const [busy, setBusy] = useState<Record<string, boolean>>({})
  const [note, setNote] = useState<{ id: string; text: string; bad: boolean } | null>(null)
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now()), 30_000)
    return () => window.clearInterval(t)
  }, [])

  const act = async (o: Orchestrator, what: 'run' | 'pause') => {
    setBusy((b) => ({ ...b, [o.id]: true }))
    setNote(null)
    try {
      if (what === 'run') {
        const r = await pluginUiPost<{ sent?: boolean }>(
          PLUGIN,
          `/orchestrators/${encodeURIComponent(o.id)}/run`,
        )
        setNote({
          id: o.id,
          text: r.sent === false ? 'Not sent (cooldown / cap)' : 'Fired',
          bad: false,
        })
      } else {
        await pluginUiPost(PLUGIN, `/orchestrators/${encodeURIComponent(o.id)}/pause`, {
          paused: !o.paused,
        })
      }
    } catch (e) {
      setNote({ id: o.id, text: e instanceof Error ? e.message : 'Failed', bad: true })
    } finally {
      setBusy((b) => ({ ...b, [o.id]: false }))
      void reload()
    }
  }

  const orchs = data?.orchestrators ?? []
  const running = orchs.filter((o) => o.brain_busy).length
  const {
    filters,
    set,
    clear,
    activeCount,
    open: filterOpen,
    toggle: toggleFilters,
  } = useWidgetFilters(props)
  const goalStates = [...new Set(orchs.map((o) => o.goal_status.state))].sort()
  const defs: FilterDef[] = [
    { key: 'q', kind: 'search', placeholder: 'Search orchestrators…' },
    { key: 'state', kind: 'chips', label: 'State', options: STATE_OPTIONS },
    {
      key: 'goal',
      kind: 'chips',
      label: 'Goal',
      options: goalStates.map((s) => ({ value: s, label: humanize(s) })),
    },
  ]
  const globalPaused = !!data?.global_paused
  const shown = orchs.filter(
    (o) =>
      inSet(filters.state, stateKey(o, globalPaused)) &&
      inSet(filters.goal, o.goal_status.state) &&
      matchesSearch(filters.q, o.name, o.goal, o.goal_status.note),
  )

  let body: ReactNode
  if (error?.notInstalled) {
    body = (
      <DashEmpty testId="dash-orchestrators-empty">
        <span data-reason="not-installed">Session Control plugin not available</span>
      </DashEmpty>
    )
  } else if (!data) {
    body = error ? (
      <DashError message={error.message} onRetry={() => void reload()} />
    ) : (
      <DashLoading />
    )
  } else if (orchs.length === 0) {
    body = <DashEmpty testId="dash-orchestrators-empty">No orchestrators</DashEmpty>
  } else if (shown.length === 0) {
    body = <DashNoMatch onClear={clear} />
  } else {
    body = (
      <div className="dash-scroll">
        {error && (
          <div className="dash-inline-error dash-banner" role="alert">
            {error.message}
          </div>
        )}
        <ul className="dash-items" data-testid="dash-orchestrators-list">
          {shown.map((o) => {
            const st = stateOf(o, data.global_paused)
            const gs = o.goal_status
            const last = o.log[o.log.length - 1]
            const open = expanded === o.id
            const isBusy = !!busy[o.id]
            return (
              <li
                key={o.id}
                className={`dash-item orch-item${st.tone === 'paused' ? ' dash-item-muted' : ''}`}
                data-testid="dash-orchestrator-row"
                data-orch-id={o.id}
                data-state={st.tone}
              >
                <div className="orch-item-top">
                  <button
                    type="button"
                    className="dash-item-main orch-main"
                    aria-expanded={open}
                    title={open ? 'Hide recent activity' : 'Show recent activity'}
                    onClick={() => setExpanded(open ? null : o.id)}
                  >
                    <span
                      className={`project-widget-dot orch-dot orch-dot-${st.tone}${st.tone === 'running' ? ' running' : ''}`}
                      role="img"
                      aria-label={st.label}
                      title={st.label}
                    />
                    <span className="dash-item-text">
                      <span className="orch-title-line">
                        <span className="dash-item-title orch-name">{o.name}</span>
                        <span
                          className={`dash-pill dash-pill-${GOAL_TONE[gs.state] ?? 'neutral'}`}
                          title={gs.note || humanize(gs.state)}
                          data-testid="dash-orchestrator-goal-status"
                        >
                          {humanize(gs.state)}
                          {gs.percent != null && ` · ${gs.percent}%`}
                        </span>
                      </span>
                      {o.goal && (
                        <span className="orch-goal" title={o.goal}>
                          {o.goal}
                        </span>
                      )}
                      <span className="dash-item-sub">
                        {o.stats.next_due_at && o.enabled && !o.paused
                          ? `Next ${relativeTime(o.stats.next_due_at, now)} · `
                          : ''}
                        Last run{' '}
                        {o.stats.last_fired_at ? relativeTime(o.stats.last_fired_at, now) : 'never'}
                        {note?.id === o.id && (
                          <span className={note.bad ? 'orch-note-bad' : undefined}>
                            {' '}
                            · {note.text}
                          </span>
                        )}
                      </span>
                      {last && (
                        <span className="dash-item-sub orch-last" title={last.detail}>
                          <span className="dash-mono">{last.kind}</span> {last.detail}
                        </span>
                      )}
                    </span>
                  </button>
                  <div className="orch-actions">
                    <button
                      type="button"
                      className="btn-secondary btn-sm"
                      disabled={isBusy}
                      aria-label={`Run ${o.name} now`}
                      title="Fire now (cooldown and caps still apply)"
                      data-testid="dash-orchestrator-run"
                      onClick={() => void act(o, 'run')}
                    >
                      Run
                    </button>
                    <button
                      type="button"
                      className="btn-secondary btn-sm"
                      disabled={isBusy}
                      aria-label={`${o.paused ? 'Resume' : 'Pause'} ${o.name}`}
                      data-testid="dash-orchestrator-pause"
                      onClick={() => void act(o, 'pause')}
                    >
                      {o.paused ? 'Resume' : 'Pause'}
                    </button>
                  </div>
                </div>
                {open && (
                  <div className="orch-detail" data-testid="dash-orchestrator-activity">
                    {o.log.length === 0 ? (
                      <span className="dash-muted">No activity yet</span>
                    ) : (
                      <ol className="orch-log">
                        {o.log
                          .slice(-RECENT_EVENTS)
                          .reverse()
                          .map((ev, i) => (
                            <li key={`${ev.ts}-${i}`} className="orch-log-row">
                              <span
                                className="dash-row-time"
                                title={logTs(ev.ts) ? new Date(ev.ts).toLocaleString() : undefined}
                              >
                                {relativeTime(logTs(ev.ts), now)}
                              </span>
                              <span className="dash-mono orch-log-kind">{ev.kind}</span>
                              <span className="orch-log-detail" title={ev.detail}>
                                {ev.detail}
                              </span>
                            </li>
                          ))}
                      </ol>
                    )}
                    {o.session_id && (
                      <button
                        type="button"
                        className="dash-link orch-brain-link"
                        onClick={() => onOpenSession(o.session_id!)}
                      >
                        Open brain session
                      </button>
                    )}
                  </div>
                )}
              </li>
            )
          })}
        </ul>
      </div>
    )
  }

  return (
    <WidgetFrame
      kind="orchestrators"
      widgetId={widget.id}
      title="Orchestrators"
      statusSlot={
        data && orchs.length > 0 ? (
          <>
            <span
              className={`dash-count${running > 0 ? ' orch-count-running' : ''}`}
              title={`${running} of ${orchs.length} running`}
            >
              {running}/{orchs.length}
            </span>
            {data.global_paused && (
              <span
                className="dash-pill dash-pill-warn"
                data-testid="dash-orchestrators-global-paused"
              >
                All paused
              </span>
            )}
            <FilterButton activeCount={activeCount} open={filterOpen} onToggle={toggleFilters} />
          </>
        ) : undefined
      }
      menuItems={menuItems}
      ctx={ctx}
    >
      {filterOpen && data && orchs.length > 0 && (
        <FilterBar defs={defs} filters={filters} set={set} clear={clear} />
      )}
      {body}
    </WidgetFrame>
  )
}
