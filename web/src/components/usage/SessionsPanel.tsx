import { useMemo, useState } from 'react'
import type { SessionUsage } from '../../types/api'
import { contextWindowFor } from '../../util/cost'
import { fmtInt, fmtTokens, fmtUsd } from '../../util/format'

/** Which figure the session list sorts on, biggest first. */
type SessionSort = 'tokens' | 'context' | 'cost'

const SORTS: { key: SessionSort; label: string }[] = [
  { key: 'tokens', label: 'Tokens' },
  { key: 'context', label: 'Context' },
  { key: 'cost', label: 'Cost' },
]

function sortValue(s: SessionUsage, key: SessionSort): number {
  if (key === 'context') return s.total_context_tokens
  if (key === 'cost') return s.est_cost
  return s.total_tokens_used
}

/** Body of the Sessions panel: a sortable list of sessions, each showing its
 *  lifetime tokens, est. cost, and a gauge of how full its context window is.
 *  Context has no model on the row, so the gauge measures against the shared
 *  default window from the cost module. */
export default function SessionsPanelBody({ sessions }: { sessions: SessionUsage[] }) {
  const [sort, setSort] = useState<SessionSort>('tokens')
  const sorted = useMemo(
    () => [...sessions].sort((a, b) => sortValue(b, sort) - sortValue(a, sort)),
    [sessions, sort],
  )

  return (
    <div className="usage-list" data-testid="usage-sessions-list">
      <div className="usage-toolbar" role="group" aria-label="Sort sessions">
        <span className="usage-toolbar-label">Sort</span>
        {SORTS.map((o) => (
          <button
            key={o.key}
            type="button"
            className={`usage-sort-btn ${sort === o.key ? 'is-active' : ''}`}
            aria-pressed={sort === o.key}
            onClick={() => setSort(o.key)}
          >
            {o.label}
          </button>
        ))}
      </div>

      {sorted.map((s) => {
        const limit = contextWindowFor(null)
        const ctx = s.total_context_tokens
        const pct = limit > 0 ? Math.min(1, ctx / limit) : 0
        const pctLabel = Math.round(pct * 100)
        const level = pct >= 0.9 ? 'is-danger' : pct >= 0.7 ? 'is-warn' : ''
        return (
          <div className="usage-row" key={s.id} data-testid="usage-session-row">
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
              aria-label={`Context ${fmtInt(ctx)} of ${fmtInt(limit)} tokens, ${pctLabel}%`}
              title={`${fmtInt(ctx)} / ${fmtInt(limit)} context tokens (${pctLabel}%)`}
            >
              <span className={`usage-gauge-fill ${level}`} style={{ width: `${pct * 100}%` }} />
            </div>
            <div className="usage-row-sub">
              Context {fmtTokens(ctx)} / {fmtTokens(limit)} ({pctLabel}%)
            </div>
          </div>
        )
      })}
    </div>
  )
}
