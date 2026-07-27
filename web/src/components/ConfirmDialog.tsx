import { useEffect, useId } from 'react'
import { createPortal } from 'react-dom'
import useDialogFocus from '../hooks/useDialogFocus'

interface ConfirmDialogProps {
  title: string
  message: string
  confirmLabel?: string
  cancelLabel?: string
  danger?: boolean
  /** Failure of the last confirm attempt. Rendering it keeps the dialog
   *  open on failure — the user reads what happened and retries in place
   *  instead of watching the dialog vanish as if it had worked. */
  error?: string | null
  /** The confirmed action is in flight: both buttons lock and Escape /
   *  backdrop clicks are ignored, so the request can't be stacked. */
  busy?: boolean
  busyLabel?: string
  /** Optional opt-out row between the message and the actions (e.g.
   *  "Don't ask again"). Supply all three checkbox props together. */
  checkboxLabel?: string
  checked?: boolean
  onCheckedChange?: (checked: boolean) => void
  testId?: string
  onConfirm: () => void
  onCancel: () => void
}

export default function ConfirmDialog({
  title,
  message,
  confirmLabel = 'Confirm',
  cancelLabel = 'Cancel',
  danger = false,
  error = null,
  busy = false,
  busyLabel = 'Working…',
  checkboxLabel,
  checked = false,
  onCheckedChange,
  testId,
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  const panelRef = useDialogFocus<HTMLDivElement>()
  const titleId = useId()
  const messageId = useId()
  useEffect(() => {
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && !busy) onCancel()
    }
    document.addEventListener('keydown', handleKey)
    return () => document.removeEventListener('keydown', handleKey)
  }, [onCancel, busy])

  return createPortal(
    <div
      className="modal-backdrop"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget && !busy) onCancel()
      }}
    >
      <div
        ref={panelRef}
        className="confirm-dialog"
        // `alertdialog` for the destructive variant: it tells a screen
        // reader this is an interruption that needs an answer, not just
        // another panel.
        role={danger ? 'alertdialog' : 'dialog'}
        aria-modal="true"
        aria-labelledby={titleId}
        aria-describedby={messageId}
        data-testid={testId}
        onMouseDown={(e) => e.stopPropagation()}
        onClick={(e) => e.stopPropagation()}
      >
        <h3 className="confirm-dialog-title" id={titleId}>
          {title}
        </h3>
        <p className="confirm-dialog-message" id={messageId}>
          {message}
        </p>
        {error && (
          <p className="confirm-dialog-error" role="alert" data-testid="confirm-dialog-error">
            {error}
          </p>
        )}
        {checkboxLabel && onCheckedChange && (
          <label className="confirm-dialog-checkbox">
            <input
              type="checkbox"
              checked={checked}
              disabled={busy}
              onChange={(e) => onCheckedChange(e.target.checked)}
              data-testid="confirm-dialog-checkbox"
            />
            <span>{checkboxLabel}</span>
          </label>
        )}
        <div className="confirm-dialog-actions">
          {/* A danger dialog opens with focus on the safe action, so an Enter
              or Space pressed straight after it appears cancels rather than
              destroys. */}
          <button
            className="btn-secondary"
            onClick={onCancel}
            disabled={busy}
            autoFocus={danger}
            data-testid="confirm-dialog-cancel"
          >
            {cancelLabel}
          </button>
          <button
            className={danger ? 'btn-primary confirm-dialog-danger' : 'btn-primary'}
            onClick={onConfirm}
            disabled={busy}
            aria-busy={busy || undefined}
            data-testid="confirm-dialog-confirm"
          >
            {busy ? busyLabel : confirmLabel}
          </button>
        </div>
      </div>
    </div>,
    document.body,
  )
}
