import type { ReactNode } from 'react'
import WidgetFrame from '../WidgetFrame'
import { formatRelativeTime } from '../../../lib/review'
import type { InfoWidgetProps } from './types'
import { useDashboardData } from './useDashboardData'
import { DashCount, DashEmpty, DashError, DashHeading, DashLoading, DashNoMatch } from './DashParts'
import {
  FilterBar,
  FilterButton,
  inSet,
  matchesSearch,
  useWidgetFilters,
  type FilterDef,
} from './filters'

interface InReview {
  card_id: string
  title: string
  project_id: string
  project_name: string
  worker_session_id: string | null
  session_running: boolean
  updated_at: string
}

interface Verdict {
  card_id: string
  title: string
  project_id: string
  project_name: string
  verdict: 'pass' | 'changes_requested'
  reviewed_at: string
  reviewer_model: string | null
  summary: string | null
}

const FILTER_DEFS: FilterDef[] = [
  { key: 'q', kind: 'search', placeholder: 'Search cards…' },
  {
    key: 'verdict',
    kind: 'chips',
    label: 'Verdict',
    options: [
      { value: 'pass', label: 'Passed' },
      { value: 'changes_requested', label: 'Changes' },
    ],
  },
]

/** Review Queue: cards in review now, plus the latest verdicts. */
export default function ReviewQueueWidget(props: InfoWidgetProps) {
  const { widget, ctx, menuItems, scopeProjectId, scopeName, onOpenSession, onOpenProject } = props
  const { data, error, reload } = useDashboardData<{ in_review: InReview[]; recent: Verdict[] }>(
    '/api/dashboard/review-queue',
    scopeProjectId,
    ['card-update', 'card-delete', 'session-updated'],
  )
  const {
    filters,
    set,
    clear,
    activeCount,
    open: filterOpen,
    toggle: toggleFilters,
  } = useWidgetFilters(props)
  // Search narrows both lists; the verdict chips only the verdicts (cards
  // still in review have none yet).
  const inReview = (data?.in_review ?? []).filter((c) =>
    matchesSearch(filters.q, c.title, c.project_name),
  )
  const recent = (data?.recent ?? []).filter(
    (v) =>
      inSet(filters.verdict, v.verdict) &&
      matchesSearch(filters.q, v.title, v.project_name, v.reviewer_model, v.summary),
  )

  let body: ReactNode
  if (!data) {
    body = error ? <DashError message={error} onRetry={() => void reload()} /> : <DashLoading />
  } else if (data.in_review.length === 0 && data.recent.length === 0) {
    body = <DashEmpty testId="dash-review_queue-empty">Nothing in review</DashEmpty>
  } else if (inReview.length === 0 && recent.length === 0) {
    body = <DashNoMatch onClear={clear} />
  } else {
    body = (
      <div className="dash-scroll" data-testid="dash-review_queue-list">
        <section className="dash-group">
          <DashHeading count={inReview.length}>In review</DashHeading>
          {inReview.length === 0 ? (
            <p className="project-widget-none">No cards in review.</p>
          ) : (
            <ul className="dash-list">
              {inReview.map((c) => {
                const sid = c.worker_session_id
                return (
                  <li key={c.card_id} data-card-id={c.card_id}>
                    <button
                      type="button"
                      className="dash-row"
                      title={sid ? 'Open reviewer session' : 'Open project board'}
                      onClick={() => (sid ? onOpenSession(sid) : onOpenProject(c.project_id))}
                    >
                      <span
                        className={`project-widget-dot${c.session_running ? ' running' : ''}`}
                        role="img"
                        aria-label={c.session_running ? 'Reviewer running' : 'Reviewer idle'}
                      />
                      <span className="dash-row-main">
                        <span className="dash-row-title">{c.title}</span>
                        {!scopeProjectId && <span className="dash-row-sub">{c.project_name}</span>}
                      </span>
                      <span className="dash-row-time">{formatRelativeTime(c.updated_at)}</span>
                    </button>
                  </li>
                )
              })}
            </ul>
          )}
        </section>
        <section className="dash-group">
          <DashHeading>Recent verdicts</DashHeading>
          {recent.length === 0 ? (
            <p className="project-widget-none">No reviews yet.</p>
          ) : (
            <ul className="dash-list">
              {recent.map((v) => (
                <li key={`${v.card_id}-${v.reviewed_at}`} data-card-id={v.card_id}>
                  <button
                    type="button"
                    className="dash-row"
                    title={v.summary ?? 'Open project board'}
                    onClick={() => onOpenProject(v.project_id)}
                  >
                    <span
                      className={`card-verdict-chip card-verdict-chip--${v.verdict}`}
                      data-verdict={v.verdict}
                    >
                      {v.verdict === 'pass' ? 'Passed' : 'Changes'}
                    </span>
                    <span className="dash-row-main">
                      <span className="dash-row-title">{v.title}</span>
                      <span className="dash-row-sub">
                        {[scopeProjectId ? null : v.project_name, v.reviewer_model]
                          .filter(Boolean)
                          .join(' · ')}
                      </span>
                    </span>
                    <span className="dash-row-time">{formatRelativeTime(v.reviewed_at)}</span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </section>
      </div>
    )
  }

  return (
    <WidgetFrame
      kind="review_queue"
      widgetId={widget.id}
      title={scopeName ? `Review Queue · ${scopeName}` : 'Review Queue'}
      statusSlot={
        data ? (
          <>
            {data.in_review.length > 0 && (
              <DashCount n={data.in_review.length} label={`${data.in_review.length} in review`} />
            )}
            <FilterButton activeCount={activeCount} open={filterOpen} onToggle={toggleFilters} />
          </>
        ) : undefined
      }
      menuItems={menuItems}
      ctx={ctx}
    >
      {filterOpen && data && (
        <FilterBar defs={FILTER_DEFS} filters={filters} set={set} clear={clear} />
      )}
      {body}
    </WidgetFrame>
  )
}
