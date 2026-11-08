import type { ReactNode } from 'react'
import HelpTip from './HelpTip'
import { docsUrl, type HelpTerm } from '../utils/helpTerms'

interface Props {
  /** Short statement of the empty state, e.g. "No projects yet". */
  title: string
  /** One plain sentence on what this page is for. */
  body?: ReactNode
  /** The page's main action, rendered as the accent button. */
  actionLabel?: string
  onAction?: () => void
  actionTestId?: string
  /** Shared glossary entry explained by a (?) tip next to the title. */
  term?: HelpTerm
  /** Docs path under `DOCS_BASE` (or a full URL) for a "Learn more" link. */
  docs?: string
  testId?: string
  /** Optional illustration shown above the title. */
  art?: ReactNode
  /** Anything else the page needs below the action (rare). */
  children?: ReactNode
}

/**
 * The empty state of a top-level page: what is missing, one sentence on what
 * the page is for, the main action, and a way to learn more. Drop it into
 * `List`'s `emptyState` slot (or anywhere a page has nothing to show) instead
 * of hand-rolling `.list-view-empty` markup.
 */
export default function EmptyState({
  title,
  body,
  actionLabel,
  onAction,
  actionTestId,
  term,
  docs,
  testId,
  art,
  children,
}: Props) {
  const href = docs ? docsUrl(docs) : undefined
  return (
    <div className="list-view-empty empty-state-block" data-testid={testId}>
      {art}
      <p className="empty-state-title">
        {title}
        {term && <HelpTip term={term} />}
      </p>
      {body && <p className="empty-state-body">{body}</p>}
      {actionLabel && onAction && (
        <button
          type="button"
          className="list-view-empty-action"
          onClick={onAction}
          data-testid={actionTestId}
        >
          {actionLabel}
        </button>
      )}
      {children}
      {href && (
        <p className="empty-state-docs">
          <a className="form-link" href={href} target="_blank" rel="noreferrer noopener">
            Learn more ↗
          </a>
        </p>
      )}
    </div>
  )
}
