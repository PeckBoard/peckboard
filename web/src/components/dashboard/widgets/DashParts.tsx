import type { ReactNode } from 'react'
import '../../../styles/dashboard-widgets.css'

/** Skeleton shown until a widget's first response lands. */
export function DashLoading() {
  return (
    <div className="project-widget-loading" aria-busy="true">
      <span className="project-widget-skeleton" />
      <span className="project-widget-skeleton" />
      <span className="project-widget-skeleton project-widget-skeleton-short" />
    </div>
  )
}

/** Load failure with a retry. */
export function DashError({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div className="split-empty-leaf" role="alert">
      <p>{message}</p>
      <button type="button" className="btn-secondary btn-sm" onClick={onRetry}>
        Retry
      </button>
    </div>
  )
}

/** Empty state; `testId` follows the `dash-<kind>-empty` contract. */
export function DashEmpty({ testId, children }: { testId: string; children: ReactNode }) {
  return (
    <div className="dash-empty" data-testid={testId}>
      {children}
    </div>
  )
}

/** Small uppercase section heading with an optional count. */
export function DashHeading({ children, count }: { children: ReactNode; count?: number }) {
  return (
    <h3 className="project-widget-heading dash-heading">
      {children}
      {count !== undefined && <span className="dash-heading-count">{count}</span>}
    </h3>
  )
}

/** Header count chip (e.g. "3 open"). */
export function DashCount({
  n,
  tone,
  label,
}: {
  n: number
  tone?: 'danger' | 'warn'
  label: string
}) {
  return (
    <span className={`dash-count${tone && n > 0 ? ` dash-count-${tone}` : ''}`} title={label}>
      {n}
    </span>
  )
}
