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

/** Projects panel body. Clicking a project selects it (click again to clear);
 *  the selection drives the cards panel's filter. */
export function ProjectsPanelBody({
  projects,
  selectedId,
  onSelect,
}: {
  projects: EntityUsage[]
  selectedId: string | null
  onSelect: (id: string | null) => void
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
          selected={p.id === selectedId}
          onClick={() => onSelect(p.id === selectedId ? null : p.id)}
        />
      ))}
    </div>
  )
}

/** Cards panel body. When a project is selected upstream, the list filters to
 *  that project's cards (via each card's `project_id`) and shows a clearable
 *  filter bar. */
export function CardsPanelBody({
  cards,
  projects,
  selectedProjectId,
  onClearFilter,
}: {
  cards: EntityUsage[]
  projects: EntityUsage[]
  selectedProjectId: string | null
  onClearFilter: () => void
}) {
  const sorted = useMemo(() => {
    const visible = selectedProjectId
      ? cards.filter((c) => c.project_id === selectedProjectId)
      : cards
    return [...visible].sort(byTokensDesc)
  }, [cards, selectedProjectId])
  const max = maxTokens(sorted)
  const projectName = selectedProjectId
    ? (projects.find((p) => p.id === selectedProjectId)?.name ?? 'selected project')
    : null

  return (
    <div className="usage-list" data-testid="usage-cards-list">
      {selectedProjectId && (
        <div className="usage-filter-bar" data-testid="usage-cards-filter">
          <span className="usage-filter-label">
            Filtered to <strong>{projectName}</strong>
          </span>
          <button type="button" className="usage-clear-btn" onClick={onClearFilter}>
            Clear
          </button>
        </div>
      )}
      {sorted.length === 0 ? (
        <div className="usage-empty-sub">No card usage in this project yet</div>
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
 *  question / pm), largest first. */
export function ExpertsPanelBody({ experts }: { experts: EntityUsage[] }) {
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
        />
      ))}
    </div>
  )
}
