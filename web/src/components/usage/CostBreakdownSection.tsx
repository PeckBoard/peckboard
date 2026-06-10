import type { OperationCost, UsageOperationKind } from '../../types/api'
import { fmtTokens, fmtUsd } from '../../util/format'

/** How many rows each breakdown panel shows before it stops. The panels are
 *  "where did the money go" leaderboards, not full tables — a handful of the
 *  priciest items is the useful view. */
const TOP_N = 8

interface KindMeta {
  kind: UsageOperationKind
  title: string
  /** Whether labels are file paths (rendered monospace, truncated from the
   *  start so the filename stays visible). */
  mono: boolean
  empty: string
}

const KINDS: KindMeta[] = [
  {
    kind: 'file_update',
    title: 'File Updates',
    mono: true,
    empty: 'No file edits recorded',
  },
  {
    kind: 'ask_expert',
    title: 'Expert Consults',
    mono: false,
    empty: 'No expert consults recorded',
  },
  { kind: 'qa', title: 'Questions & Answers', mono: false, empty: 'No Q&A recorded' },
]

function CostPanel({ meta, ops }: { meta: KindMeta; ops: OperationCost[] }) {
  const rows = [...ops].sort((a, b) => b.est_cost - a.est_cost)
  const subtotal = rows.reduce((s, r) => s + r.est_cost, 0)
  const top = rows.slice(0, TOP_N)
  const maxCost = top.length > 0 ? Math.max(...top.map((r) => r.est_cost), 0) : 0
  const testid = `usage-cost-${meta.kind}`

  return (
    <section className="usage-panel usage-cost-panel" data-testid={testid}>
      <header className="usage-panel-header">
        <h4 className="usage-panel-title">{meta.title}</h4>
        <span className="usage-cost-subtotal" data-testid={`${testid}-subtotal`}>
          {fmtUsd(subtotal)}
        </span>
      </header>
      <div className="usage-cost-body">
        {top.length === 0 ? (
          <div className="usage-panel-empty">{meta.empty}</div>
        ) : (
          <ol className="usage-op-list">
            {top.map((op) => (
              <li className="usage-op-row" key={`${op.kind}:${op.ref_id}`}>
                <span
                  className={meta.mono ? 'usage-op-label usage-op-label-path' : 'usage-op-label'}
                  title={op.label}
                >
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
        {rows.length > TOP_N && <div className="usage-op-more">+{rows.length - TOP_N} more</div>}
      </div>
    </section>
  )
}

/** The cost-and-trends card's first half: a top-N "where the spend went" panel
 *  for each operation kind, fed by the operations the store already fetched
 *  from `/api/usage/operations`. */
export default function CostBreakdownSection({ operations }: { operations: OperationCost[] }) {
  return (
    <section className="usage-section" data-testid="usage-cost-breakdown">
      <h3 className="usage-section-title">Cost Breakdown</h3>
      <div className="usage-subgrid">
        {KINDS.map((meta) => (
          <CostPanel
            key={meta.kind}
            meta={meta}
            ops={operations.filter((o) => o.kind === meta.kind)}
          />
        ))}
      </div>
    </section>
  )
}
