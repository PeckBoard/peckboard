import { REVIEW_STATUS_LABEL, type ReviewStatus } from '../../lib/review'

interface Props {
  status: ReviewStatus
  /** Set only on the review screen's chip — the list rows leave it off so
   *  the id stays unique per page (see the feature's testid contract). */
  testId?: string
}

/**
 * The review lifecycle chip: annotating · running (with a spinner) ·
 * needs input · approved. Shared by the list rows and the review screen so
 * one status never reads two different ways.
 */
export default function ReviewStatusChip({ status, testId }: Props) {
  return (
    <span
      className={`review-status review-status--${status}`}
      data-testid={testId}
      data-status={status}
    >
      {status === 'running' && <span className="review-status__spinner" aria-hidden="true" />}
      {REVIEW_STATUS_LABEL[status]}
    </span>
  )
}
