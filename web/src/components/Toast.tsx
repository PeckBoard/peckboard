import { useEffect } from 'react'
import '../styles/toast.css'

/** A short-lived notice pinned to the bottom of the screen, with one
 *  optional action (e.g. "Back") and a dismiss button. Auto-hides after
 *  `timeoutMs`. Announced politely to screen readers. */
export default function Toast({
  message,
  actionLabel,
  onAction,
  onDismiss,
  timeoutMs = 8000,
  testId,
}: {
  message: string
  actionLabel?: string
  onAction?: () => void
  onDismiss: () => void
  timeoutMs?: number
  testId?: string
}) {
  useEffect(() => {
    const t = window.setTimeout(onDismiss, timeoutMs)
    return () => window.clearTimeout(t)
  }, [message, onDismiss, timeoutMs])

  return (
    <div className="toast" role="status" aria-live="polite" data-testid={testId}>
      <span className="toast-message">{message}</span>
      {actionLabel && onAction && (
        <button
          type="button"
          className="btn-secondary toast-action"
          onClick={onAction}
          data-testid={testId ? `${testId}-action` : undefined}
        >
          {actionLabel}
        </button>
      )}
      <button type="button" className="toast-dismiss" aria-label="Dismiss" onClick={onDismiss}>
        ×
      </button>
    </div>
  )
}
