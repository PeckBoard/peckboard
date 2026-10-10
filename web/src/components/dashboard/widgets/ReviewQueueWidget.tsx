import type { ReactNode } from 'react'
import WidgetFrame from '../WidgetFrame'
import { formatRelativeTime } from '../../../lib/review'
import type { InfoWidgetProps } from './types'
import { useDashboardData } from './useDashboardData'
import { DashCount, DashEmpty, DashError, DashHeading, DashLoading } from './DashParts'

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

/** Review Queue: cards in review now, plus the latest verdicts. */
export default function ReviewQueueWidget({
  widget,
  ctx,
  menuItems,
  scopeProjectId,
  scopeName,
  onOpenSession,
  onOpenProject,
}: InfoWidgetProps) {
  const { data, error, reload } = useDashboardData<{ in_review: InReview[]; recent: Verdict[] }>(
    '/api/dashboard/review-queue',
    scopeProjectId,
    ['card-update', 'card-delete', 'session-updated'],
  )

  let body: ReactNode
  if (!data) {
    body = error ? <DashError message={error} onRetry={() => void reload()} /> : <DashLoading />
  } else if (data.in_review.length === 0 && data.recent.length === 0) {
    body = <DashEmpty testId="dash-review_queue-empty">Nothing in review</DashEmpty>
  } else {
    body = (
      <div className="dash-scroll" data-testid="dash-review_queue-list">
        <section className="dash-group">
          <DashHeading count={data.in_review.length}>In review</DashHeading>
          {data.in_review.length === 0 ? (
            <p className="project-widget-none">No cards in review.</p>
          ) : (
            <ul className="dash-list">
              {data.in_review.map((c) => {
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
          {data.recent.length === 0 ? (
            <p className="project-widget-none">No reviews yet.</p>
          ) : (
            <ul className="dash-list">
              {data.recent.map((v) => (
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
        data && data.in_review.length > 0 ? (
          <DashCount n={data.in_review.length} label={`${data.in_review.length} in review`} />
        ) : undefined
      }
      menuItems={menuItems}
      ctx={ctx}
    >
      {body}
    </WidgetFrame>
  )
}
