import { useEffect, useMemo, useState } from 'react'
import { fetchOperationCosts, fetchTrendSeries } from '../../store/usage'
import type { EntityUsage, OperationCost, SessionUsage, TrendSeries } from '../../types/api'
import { fmtTokens, fmtUsd } from '../../util/format'
import LineChart, { type ChartSeries } from './LineChart'

function fmtDay(ts: number): string {
  const d = new Date(ts)
  return `${`${d.getMonth() + 1}`.padStart(2, '0')}/${`${d.getDate()}`.padStart(2, '0')}`
}

function SessionList({
  title,
  rows,
  testid,
  onOpenSession,
}: {
  title: string
  rows: SessionUsage[]
  testid: string
  onOpenSession: (id: string) => void
}) {
  const max = rows.reduce((m, r) => Math.max(m, r.total_tokens_used), 1)
  return (
    <section className="usage-panel" data-testid={testid}>
      <header className="usage-panel-header">
        <h3 className="usage-panel-title">{title}</h3>
        <span className="usage-panel-count">{rows.length}</span>
      </header>
      <div className="usage-panel-body">
        {rows.length === 0 ? (
          <div className="usage-panel-empty">No data yet</div>
        ) : (
          <div className="usage-list">
            {rows.map((s) => (
              <div
                className="usage-row usage-row-clickable"
                key={s.id}
                data-testid={`${testid}-row`}
                role="button"
                tabIndex={0}
                onClick={() => onOpenSession(s.id)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault()
                    onOpenSession(s.id)
                  }
                }}
              >
                <div className="usage-row-head">
                  <span className="usage-row-name" title={s.name}>
                    {s.name || 'Untitled'}
                  </span>
                  <span className="usage-row-figs">
                    {fmtTokens(s.total_tokens_used)} · {fmtUsd(s.est_cost)}
                  </span>
                </div>
                <div
                  className="usage-gauge"
                  role="img"
                  aria-label={`${s.total_tokens_used} tokens`}
                >
                  <span
                    className="usage-gauge-fill"
                    style={{ width: `${(s.total_tokens_used / max) * 100}%` }}
                  />
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </section>
  )
}

function OpsPanel({ title, ops, testid }: { title: string; ops: OperationCost[]; testid: string }) {
  const rows = [...ops].sort((a, b) => b.est_cost - a.est_cost).slice(0, 8)
  const maxCost = rows.length > 0 ? Math.max(...rows.map((r) => r.est_cost)) : 0
  return (
    <section className="usage-panel usage-cost-panel" data-testid={testid}>
      <header className="usage-panel-header">
        <h4 className="usage-panel-title">{title}</h4>
        <span className="usage-cost-subtotal">
          {fmtUsd(ops.reduce((s, r) => s + r.est_cost, 0))}
        </span>
      </header>
      <div className="usage-cost-body">
        {rows.length === 0 ? (
          <div className="usage-panel-empty">Nothing recorded</div>
        ) : (
          <ol className="usage-op-list">
            {rows.map((op) => (
              <li className="usage-op-row" key={`${op.kind}:${op.ref_id}`}>
                <span className="usage-op-label usage-op-label-path" title={op.label}>
                  {op.label}
                </span>
                <span className="usage-op-figs">
                  <span className="usage-op-tokens">{fmtTokens(op.tokens)} tok</span>
                  <span className="usage-op-cost">{fmtUsd(op.est_cost)}</span>
                </span>
                <span className="usage-op-bar" aria-hidden="true">
                  <span
                    className="usage-op-bar-fill"
                    style={{ width: maxCost > 0 ? `${(op.est_cost / maxCost) * 100}%` : '0%' }}
                  />
                </span>
              </li>
            ))}
          </ol>
        )}
      </div>
    </section>
  )
}

/** Per-project usage page: the project's totals, every session that spent
 *  tokens under it (chats and workers, each opening its own per-prompt page),
 *  its cards, what files its spend updated/re-read, and its token trend. */
export default function ProjectDetail({
  id,
  project,
  sessions,
  cards,
  onBack,
  onOpenSession,
}: {
  id: string
  /** The project's rollup row from the dashboard (null if it has none). */
  project: EntityUsage | null
  /** All session rows; this component filters to the project's. */
  sessions: SessionUsage[]
  cards: EntityUsage[]
  onBack: () => void
  onOpenSession: (id: string) => void
}) {
  const [ops, setOps] = useState<{ updates: OperationCost[]; reads: OperationCost[] }>({
    updates: [],
    reads: [],
  })
  const [trend, setTrend] = useState<TrendSeries[]>([])

  useEffect(() => {
    let cancelled = false
    Promise.all([
      fetchOperationCosts('file_update', { projectId: id }),
      fetchOperationCosts('file_read', { projectId: id }),
      fetchTrendSeries({ metric: 'tokens', entity: 'project', id, bucket: 'day' }),
    ]).then(([updates, reads, series]) => {
      if (!cancelled) {
        setOps({ updates, reads })
        setTrend(series)
      }
    })
    return () => {
      cancelled = true
    }
  }, [id])

  const projectSessions = useMemo(() => sessions.filter((s) => s.project_id === id), [sessions, id])
  const chats = projectSessions.filter((s) => !s.is_worker && !s.is_expert)
  const workers = projectSessions.filter((s) => s.is_worker)
  const projectCards = useMemo(() => cards.filter((c) => c.project_id === id), [cards, id])

  const stats = project
    ? [
        { label: 'Est. Cost', value: fmtUsd(project.est_cost) },
        { label: 'Total Tokens', value: fmtTokens(project.total_tokens) },
        { label: 'Input', value: fmtTokens(project.input_tokens) },
        { label: 'Output', value: fmtTokens(project.output_tokens) },
        { label: 'Cache Read', value: fmtTokens(project.cache_read_tokens) },
        { label: 'Sessions', value: `${projectSessions.length}` },
      ]
    : []

  const chartSeries: ChartSeries[] = trend.map((s) => ({
    id: s.entity_id,
    label: project?.name ?? 'Project',
    color: 'var(--accent)',
    points: s.points.map((p) => ({ x: p.bucket_ts, y: p.tokens })),
  }))

  const maxCardTokens = projectCards.reduce((m, c) => Math.max(m, c.total_tokens), 1)

  return (
    <div className="usage-detail" data-testid="usage-project-detail">
      <div className="usage-detail-header">
        <button type="button" className="usage-back-btn" onClick={onBack}>
          ← Usage
        </button>
        <h2 className="usage-title">{project?.name || 'Project'}</h2>
        <span className="usage-kind-badge usage-kind-project">Project</span>
      </div>

      <div className="usage-stat-grid" data-testid="usage-project-totals">
        {stats.map((c) => (
          <div className="usage-stat-card" key={c.label}>
            <div className="usage-stat-label">{c.label}</div>
            <div className="usage-stat-value">{c.value}</div>
          </div>
        ))}
      </div>

      <div className="usage-grid">
        <SessionList
          title="Chats"
          rows={chats}
          testid="usage-project-chats"
          onOpenSession={onOpenSession}
        />
        <SessionList
          title="Workers"
          rows={workers}
          testid="usage-project-workers"
          onOpenSession={onOpenSession}
        />
        <section className="usage-panel" data-testid="usage-project-cards">
          <header className="usage-panel-header">
            <h3 className="usage-panel-title">Cards</h3>
            <span className="usage-panel-count">{projectCards.length}</span>
          </header>
          <div className="usage-panel-body">
            {projectCards.length === 0 ? (
              <div className="usage-panel-empty">No card usage yet</div>
            ) : (
              <div className="usage-list">
                {projectCards.map((c) => (
                  <div className="usage-row" key={c.id}>
                    <div className="usage-row-head">
                      <span className="usage-row-name" title={c.name}>
                        {c.name || 'Untitled'}
                      </span>
                      <span className="usage-row-figs">
                        {fmtTokens(c.total_tokens)} · {fmtUsd(c.est_cost)}
                      </span>
                    </div>
                    <div className="usage-gauge" role="img" aria-label={`${c.total_tokens} tokens`}>
                      <span
                        className="usage-gauge-fill"
                        style={{ width: `${(c.total_tokens / maxCardTokens) * 100}%` }}
                      />
                    </div>
                  </div>
                ))}
              </div>
            )}
          </div>
        </section>
        <section className="usage-panel" data-testid="usage-project-trend">
          <header className="usage-panel-header">
            <h3 className="usage-panel-title">Tokens Over Time</h3>
          </header>
          <div className="usage-panel-body">
            <LineChart series={chartSeries} area formatValue={fmtTokens} formatX={fmtDay} />
          </div>
        </section>
      </div>

      <div className="usage-subgrid">
        <OpsPanel title="File Updates" ops={ops.updates} testid="usage-project-file-updates" />
        <OpsPanel title="Cache Reads by File" ops={ops.reads} testid="usage-project-file-reads" />
      </div>
    </div>
  )
}
