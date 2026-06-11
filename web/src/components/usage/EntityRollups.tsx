import { useMemo } from 'react'
import type { EntityUsage } from '../../types/api'
import { fmtInt, fmtTokens, fmtUsd } from '../../util/format'

/** One entity row (project / card / expert): name, token + cost figures, and
 *  a bar showing its share of the largest entity in the same list. When
 *  `onClick` is supplied the row is a keyboard-activatable button and can show
 *  a selected state. */
function EntityRow({
  name,
  tokens,
  cost,
  share,
  testid,
  selected,
  onClick,
}: {
  name: string
  tokens: number
  cost: number
  share: number
  testid: string
  selected?: boolean
  onClick?: () => void
}) {
  const clickable = !!onClick
  return (
    <div
      className={`usage-row ${clickable ? 'usage-row-clickable' : ''} ${selected ? 'is-selected' : ''}`}
      data-testid={testid}
      role={clickable ? 'button' : undefined}
      tabIndex={clickable ? 0 : undefined}
      aria-pressed={clickable ? !!selected : undefined}
      onClick={onClick}
      onKeyDown={
        clickable
          ? (e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault()
                onClick?.()
              }
            }
          : undefined
      }
    >
      <div className="usage-row-head">
        <span className="usage-row-name" title={name}>
          {name || 'Untitled'}
        </span>
        <span className="usage-row-figs">
          {fmtTokens(tokens)} · {fmtUsd(cost)}
        </span>
      </div>
      <div className="usage-gauge" role="img" aria-label={`${fmtInt(tokens)} tokens`}>
        <span className="usage-gauge-fill" style={{ width: `${share * 100}%` }} />
      </div>
    </div>
  )
}

/** Largest `total_tokens` in a set, floored at 1 so a share is never divided
 *  by zero. */
function maxTokens(rows: EntityUsage[]): number {
  return rows.reduce((m, r) => Math.max(m, r.total_tokens), 1)
}

function byTokensDesc(a: EntityUsage, b: EntityUsage): number {
  return b.total_tokens - a.total_tokens
}

/** Projects panel body. Clicking a project opens its per-project usage page
 *  (sessions, workers, cards, file spend, trend). */
export function ProjectsPanelBody({
  projects,
  onOpen,
}: {
  projects: EntityUsage[]
  onOpen?: (id: string) => void
}) {
  const sorted = useMemo(() => [...projects].sort(byTokensDesc), [projects])
  const max = maxTokens(sorted)
  return (
    <div className="usage-list" data-testid="usage-projects-list">
      {sorted.map((p) => (
        <EntityRow
          key={p.id}
          testid="usage-project-row"
          name={p.name}
          tokens={p.total_tokens}
          cost={p.est_cost}
          share={p.total_tokens / max}
          onClick={onOpen ? () => onOpen(p.id) : undefined}
        />
      ))}
    </div>
  )
}

/** Cards panel body. Project-scoped card views live on the per-project usage
 *  page; this overview list shows every card's spend, largest first. */
export function CardsPanelBody({ cards }: { cards: EntityUsage[] }) {
  const sorted = useMemo(() => [...cards].sort(byTokensDesc), [cards])
  const max = maxTokens(sorted)

  return (
    <div className="usage-list" data-testid="usage-cards-list">
      {sorted.length === 0 ? (
        <div className="usage-empty-sub">No card usage yet</div>
      ) : (
        sorted.map((c) => (
          <EntityRow
            key={c.id}
            testid="usage-card-row"
            name={c.name}
            tokens={c.total_tokens}
            cost={c.est_cost}
            share={c.total_tokens / max}
          />
        ))
      )}
    </div>
  )
}

/** Experts panel body: tokens used by each expert session (knowledge /
 *  question / pm), largest first. Experts are sessions, so `onOpen` routes to
 *  the same per-session detail page. */
export function ExpertsPanelBody({
  experts,
  onOpen,
}: {
  experts: EntityUsage[]
  onOpen?: (id: string) => void
}) {
  const sorted = useMemo(() => [...experts].sort(byTokensDesc), [experts])
  const max = maxTokens(sorted)
  return (
    <div className="usage-list" data-testid="usage-experts-list">
      {sorted.map((e) => (
        <EntityRow
          key={e.id}
          testid="usage-expert-row"
          name={e.name}
          tokens={e.total_tokens}
          cost={e.est_cost}
          share={e.total_tokens / max}
          onClick={onOpen ? () => onOpen(e.id) : undefined}
        />
      ))}
    </div>
  )
}
