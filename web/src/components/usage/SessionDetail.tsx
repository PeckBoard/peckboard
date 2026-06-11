import { useEffect, useState } from 'react'
import { fetchOperationCosts, fetchSessionTurns, fetchSessionUsage } from '../../store/usage'
import type { OperationCost, SessionUsage, TurnUsage } from '../../types/api'
import { contextWindowFor } from '../../util/cost'
import { fmtInt, fmtTokens, fmtUsd } from '../../util/format'
import LineChart, { type ChartSeries } from './LineChart'

/** Everything the per-session usage page needs, fetched together. */
interface Detail {
  usage: SessionUsage | null
  turns: TurnUsage[]
  fileReads: OperationCost[]
}

function fmtTime(ts: number): string {
  const d = new Date(ts)
  const mm = `${d.getMonth() + 1}`.padStart(2, '0')
  const dd = `${d.getDate()}`.padStart(2, '0')
  const hh = `${d.getHours()}`.padStart(2, '0')
  const mi = `${d.getMinutes()}`.padStart(2, '0')
  return `${mm}/${dd} ${hh}:${mi}`
}

/** Bare model id for display — drops the `claude:` provider prefix. */
function bareModel(model: string | null): string {
  if (!model) return '—'
  return model.startsWith('claude:') ? model.slice('claude:'.length) : model
}

/** The most recent turn's model — what the context-window gauge should be
 *  measured against, since occupancy is a now-snapshot. */
function latestModel(turns: TurnUsage[]): string | null {
  for (let i = turns.length - 1; i >= 0; i--) {
    if (turns[i].model) return turns[i].model
  }
  return null
}

function ContextGauge({ context, model }: { context: number; model: string | null }) {
  const limit = contextWindowFor(model)
  const pct = limit > 0 ? Math.min(1, context / limit) : 0
  const pctLabel = Math.round(pct * 100)
  const level = pct >= 0.9 ? 'is-danger' : pct >= 0.7 ? 'is-warn' : ''
  return (
    <div className="usage-detail-context" data-testid="usage-detail-context">
      <div className="usage-row-head">
        <span className="usage-row-name">Context window ({bareModel(model)})</span>
        <span className="usage-row-figs">
          {fmtTokens(context)} / {fmtTokens(limit)} ({pctLabel}%)
        </span>
      </div>
      <div
        className="usage-gauge"
        role="img"
        aria-label={`Context ${fmtInt(context)} of ${fmtInt(limit)} tokens, ${pctLabel}%`}
        title={`${fmtInt(context)} / ${fmtInt(limit)} context tokens (${pctLabel}%)`}
      >
        <span className={`usage-gauge-fill ${level}`} style={{ width: `${pct * 100}%` }} />
      </div>
    </div>
  )
}

function TurnRow({ turn }: { turn: TurnUsage }) {
  const hasFiles = turn.files_read.length > 0 || turn.files_edited.length > 0
  return (
    <details className="usage-turn" data-testid="usage-turn-row">
      <summary className="usage-turn-summary">
        <span className="usage-turn-seq">#{turn.turn_seq ?? '?'}</span>
        <span className="usage-turn-prompt" title={turn.prompt ?? undefined}>
          {turn.prompt ?? <em>(no prompt — resumed or kicked off automatically)</em>}
        </span>
        <span className="usage-turn-figs">
          <span title="Tokens this turn">{fmtTokens(turn.total_tokens)}</span>
          <span title="Cache read tokens" className="usage-turn-cache">
            ⟳ {fmtTokens(turn.cache_read_tokens)}
          </span>
          <span title="Estimated cost">{fmtUsd(turn.est_cost)}</span>
        </span>
      </summary>
      <div className="usage-turn-body">
        <div className="usage-turn-meta">
          <span>{fmtTime(turn.ts)}</span>
          <span>{bareModel(turn.model)}</span>
          <span>in {fmtInt(turn.input_tokens)}</span>
          <span>out {fmtInt(turn.output_tokens)}</span>
          <span>cache read {fmtInt(turn.cache_read_tokens)}</span>
          <span>cache write {fmtInt(turn.cache_creation_tokens)}</span>
          <span>context after {fmtTokens(turn.context_tokens)}</span>
        </div>
        {turn.models.length > 1 && (
          <div className="usage-turn-models" data-testid="usage-turn-models">
            {turn.models.map((m) => (
              <div className="usage-turn-meta" key={m.model ?? 'unknown'}>
                <span>{bareModel(m.model)}</span>
                <span>in {fmtInt(m.input_tokens)}</span>
                <span>out {fmtInt(m.output_tokens)}</span>
                <span>cache read {fmtInt(m.cache_read_tokens)}</span>
                <span>cache write {fmtInt(m.cache_creation_tokens)}</span>
              </div>
            ))}
          </div>
        )}
        {hasFiles ? (
          <div className="usage-turn-files">
            {turn.files_read.length > 0 && (
              <div data-testid="usage-turn-files-read">
                <div className="usage-turn-files-title">
                  Files read ({turn.files_read.length}) — cache-read spend{' '}
                  {fmtTokens(turn.cache_read_tokens)} tok
                </div>
                <ul>
                  {turn.files_read.map((f) => (
                    <li key={f} className="usage-op-label-path" title={f}>
                      {f}
                    </li>
                  ))}
                </ul>
              </div>
            )}
            {turn.files_edited.length > 0 && (
              <div data-testid="usage-turn-files-edited">
                <div className="usage-turn-files-title">
                  Files edited ({turn.files_edited.length})
                </div>
                <ul>
                  {turn.files_edited.map((f) => (
                    <li key={f} className="usage-op-label-path" title={f}>
                      {f}
                    </li>
                  ))}
                </ul>
              </div>
            )}
          </div>
        ) : (
          <div className="usage-empty-sub">No file reads or edits this turn</div>
        )}
      </div>
    </details>
  )
}

/** Per-session usage page: lifetime rollup, live context-window occupancy,
 *  context growth across turns, the per-prompt (per-turn) breakdown with the
 *  files each turn read/edited, and where the session's cache-read spend
 *  went, by file. Used for chat sessions, workers, and experts alike. */
export default function SessionDetail({ id, onBack }: { id: string; onBack: () => void }) {
  // Loaded data tagged with the session it belongs to; "loading" is derived
  // (tag !== current id) rather than set synchronously in the effect, which
  // would trigger cascading renders (same pattern as TrendsSection).
  const [loaded, setLoaded] = useState<{ key: string; detail: Detail } | null>(null)

  useEffect(() => {
    let cancelled = false
    Promise.all([
      fetchSessionUsage(id),
      fetchSessionTurns(id),
      fetchOperationCosts('file_read', { sessionId: id }),
    ]).then(([usage, turns, fileReads]) => {
      if (!cancelled) setLoaded({ key: id, detail: { usage, turns, fileReads } })
    })
    return () => {
      cancelled = true
    }
  }, [id])

  const detail = loaded?.key === id ? loaded.detail : null

  if (!detail) {
    return (
      <div className="usage-detail" data-testid="usage-session-detail">
        <div className="chat-loading">
          <div className="loading-spinner" />
        </div>
      </div>
    )
  }

  const { usage, turns, fileReads } = detail
  const model = latestModel(turns)
  const latestContext =
    turns.length > 0 ? turns[turns.length - 1].context_tokens : (usage?.total_context_tokens ?? 0)

  const kindLabel = usage?.is_expert ? 'Expert' : usage?.is_worker ? 'Worker' : 'Chat'

  const stats = usage
    ? [
        { label: 'Est. Cost', value: fmtUsd(usage.est_cost) },
        { label: 'Total Tokens', value: fmtTokens(usage.total_tokens_used) },
        { label: 'Input', value: fmtTokens(usage.input_tokens) },
        { label: 'Output', value: fmtTokens(usage.output_tokens) },
        { label: 'Cache Read', value: fmtTokens(usage.cache_read_tokens) },
        { label: 'Cache Write', value: fmtTokens(usage.cache_creation_tokens) },
        { label: 'Turns', value: `${turns.length}` },
      ]
    : []

  const contextSeries: ChartSeries[] = [
    {
      id: 'context',
      label: 'Context after turn',
      color: 'var(--accent)',
      points: turns.map((t, i) => ({ x: t.turn_seq ?? i + 1, y: t.context_tokens })),
    },
  ]

  const sortedReads = [...fileReads].sort((a, b) => b.est_cost - a.est_cost)
  const maxReadCost = sortedReads.length > 0 ? Math.max(...sortedReads.map((r) => r.est_cost)) : 0

  return (
    <div className="usage-detail" data-testid="usage-session-detail">
      <div className="usage-detail-header">
        <button type="button" className="usage-back-btn" onClick={onBack}>
          ← Usage
        </button>
        <h2 className="usage-title">{usage?.name || 'Untitled session'}</h2>
        <span className={`usage-kind-badge usage-kind-${kindLabel.toLowerCase()}`}>
          {kindLabel}
        </span>
      </div>

      <div className="usage-stat-grid" data-testid="usage-detail-totals">
        {stats.map((c) => (
          <div className="usage-stat-card" key={c.label}>
            <div className="usage-stat-label">{c.label}</div>
            <div className="usage-stat-value">{c.value}</div>
          </div>
        ))}
      </div>

      <ContextGauge context={latestContext} model={model} />

      {turns.length > 1 && (
        <section className="usage-panel" data-testid="usage-context-chart">
          <header className="usage-panel-header">
            <h3 className="usage-panel-title">Context Growth by Turn</h3>
          </header>
          <div className="usage-panel-body">
            <LineChart
              series={contextSeries}
              area
              formatValue={fmtTokens}
              formatX={(x) => `#${Math.round(x)}`}
            />
          </div>
        </section>
      )}

      <section className="usage-panel" data-testid="usage-turns-panel">
        <header className="usage-panel-header">
          <h3 className="usage-panel-title">Turns</h3>
          <span className="usage-panel-count">{turns.length}</span>
        </header>
        <div className="usage-panel-body">
          {turns.length === 0 ? (
            <div className="usage-panel-empty">No usage recorded yet</div>
          ) : (
            <div className="usage-turn-list">
              {[...turns].reverse().map((t) => (
                <TurnRow key={`${t.ts}:${t.turn_seq ?? 0}`} turn={t} />
              ))}
            </div>
          )}
        </div>
      </section>

      <section className="usage-panel" data-testid="usage-cache-reads-panel">
        <header className="usage-panel-header">
          <h3 className="usage-panel-title">Cache Reads by File</h3>
          <span className="usage-panel-count">{sortedReads.length}</span>
        </header>
        <div className="usage-panel-body">
          {sortedReads.length === 0 ? (
            <div className="usage-panel-empty">No file reads recorded</div>
          ) : (
            <ol className="usage-op-list">
              {sortedReads.map((op) => (
                <li className="usage-op-row" key={op.ref_id}>
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
                      style={{
                        width: maxReadCost > 0 ? `${(op.est_cost / maxReadCost) * 100}%` : '0%',
                      }}
                    />
                  </span>
                </li>
              ))}
            </ol>
          )}
        </div>
      </section>
    </div>
  )
}
