import { useEffect, useState, type ReactNode } from 'react'
import WidgetFrame from '../WidgetFrame'
import { useUsageStore } from '../../../store/usage'
import { bareModelId, contextWindowInfo } from '../../../util/cost'
import type { InfoWidgetProps } from './types'
import { fmtElapsed, humanize, useDashboardData } from './useDashboardData'
import { DashCount, DashEmpty, DashError, DashLoading } from './DashParts'

interface Worker {
  session_id: string
  session_name: string
  project_id: string
  project_name: string
  card_id: string
  card_title: string
  step: string
  model: string | null
  started_at: string | null
  running: boolean
  last_activity_at: string | null
  context_tokens: number | null
}

/** Worker Fleet: every card with a worker, its step, model, runtime and
 *  context fill. Rows open the worker session. */
export default function WorkersWidget({
  widget,
  ctx,
  menuItems,
  scopeProjectId,
  scopeName,
  onOpenSession,
}: InfoWidgetProps) {
  const { data, error, reload } = useDashboardData<{ workers: Worker[] }>(
    '/api/dashboard/workers',
    scopeProjectId,
    ['card-update', 'card-delete', 'session-updated'],
  )
  // The rate table is how `contextWindowInfo` recognizes a standard tier.
  const costTable = useUsageStore((s) => s.costTable)
  const fetchCostTable = useUsageStore((s) => s.fetchCostTable)
  useEffect(() => {
    void fetchCostTable()
  }, [fetchCostTable])

  const workers = data?.workers ?? []
  const running = workers.filter((w) => w.running).length
  // Tick once a second only while something is running.
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (running === 0) return
    const t = window.setInterval(() => setNow(Date.now()), 1000)
    return () => window.clearInterval(t)
  }, [running])

  let body: ReactNode
  if (!data) {
    body = error ? <DashError message={error} onRetry={() => void reload()} /> : <DashLoading />
  } else if (workers.length === 0) {
    body = <DashEmpty testId="dash-workers-empty">No workers assigned</DashEmpty>
  } else {
    body = (
      <div className="dash-scroll">
        <table className="dash-table" data-testid="dash-workers-list">
          <thead>
            <tr>
              <th aria-label="State" />
              <th>Card</th>
              <th>Step</th>
              <th>Model</th>
              <th className="dash-num">Elapsed</th>
              <th>Context</th>
            </tr>
          </thead>
          <tbody>
            {workers.map((w) => {
              const started = w.started_at ? Date.parse(w.started_at) : NaN
              const elapsed = Number.isFinite(started) ? fmtElapsed(now - started) : '—'
              const { limit, known } = contextWindowInfo(w.model, costTable)
              const pct =
                w.context_tokens !== null && limit > 0
                  ? Math.min(1, w.context_tokens / limit)
                  : null
              const level =
                pct === null ? '' : pct >= 0.9 ? ' is-danger' : pct >= 0.7 ? ' is-warn' : ''
              return (
                <tr
                  key={w.session_id}
                  className="dash-table-row"
                  tabIndex={0}
                  role="button"
                  data-session-id={w.session_id}
                  title={`Open ${w.session_name}`}
                  onClick={() => onOpenSession(w.session_id)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' || e.key === ' ') {
                      e.preventDefault()
                      onOpenSession(w.session_id)
                    }
                  }}
                >
                  <td>
                    <span
                      className={`project-widget-dot${w.running ? ' running' : ''}`}
                      role="img"
                      aria-label={w.running ? 'Running' : 'Idle'}
                    />
                  </td>
                  <td className="dash-cell-title">
                    <span className="dash-row-title">{w.card_title}</span>
                    {!scopeProjectId && <span className="dash-row-sub">{w.project_name}</span>}
                  </td>
                  <td>
                    <span className="project-widget-card-step">{humanize(w.step)}</span>
                  </td>
                  <td className="dash-mono dash-cell-model" title={w.model ?? undefined}>
                    {w.model ? bareModelId(w.model) : '—'}
                  </td>
                  <td className="dash-num">
                    {w.running ? elapsed : <span className="dash-muted">idle</span>}
                  </td>
                  <td>
                    {pct !== null && (
                      <span
                        className="dash-ctx"
                        title={`${(w.context_tokens ?? 0).toLocaleString()} / ${limit.toLocaleString()} tokens${known ? '' : ' (default window — model unknown)'}`}
                      >
                        <span className="dash-ctx-bar">
                          <span
                            className={`dash-ctx-fill${level}`}
                            style={{ width: `${pct * 100}%` }}
                          />
                        </span>
                        <span className="dash-ctx-pct">{Math.round(pct * 100)}%</span>
                      </span>
                    )}
                  </td>
                </tr>
              )
            })}
          </tbody>
        </table>
      </div>
    )
  }

  return (
    <WidgetFrame
      kind="workers"
      widgetId={widget.id}
      title={scopeName ? `Worker Fleet · ${scopeName}` : 'Worker Fleet'}
      statusSlot={
        data && workers.length > 0 ? (
          <DashCount n={running} label={`${running} of ${workers.length} running`} />
        ) : undefined
      }
      menuItems={menuItems}
      ctx={ctx}
    >
      {body}
    </WidgetFrame>
  )
}
