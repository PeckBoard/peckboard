import { useId } from 'react'
import Modal from './Modal'

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
  /** Optional third button, rendered between Cancel and Confirm, for a
   *  dialog that offers two ways forward rather than one (e.g. hand over
   *  the context vs clear it). */
  secondaryAction?: { label: string; onSelect: () => void; testId?: string }
  testId?: string
  /** Override the confirm button's testid. Defaults to the shared
   *  `confirm-dialog-confirm`. */
  confirmTestId?: string
  onConfirm: () => void
  onCancel: () => void
}

/**
 * The app's confirmation dialog. A thin skin over `Modal` — the backdrop,
 * portal, Escape handling and focus trap/restore all live there, so this
 * file only owns the title/message/actions layout. Never hand-roll a
 * second confirmation dialog; extend this one.
 */
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
  secondaryAction,
  testId,
  confirmTestId,
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  const titleId = useId()
  const messageId = useId()

  return (
    <Modal
      className="confirm-dialog"
      // `alertdialog` for the destructive variant: it tells a screen
      // reader this is an interruption that needs an answer, not just
      // another panel.
      role={danger ? 'alertdialog' : 'dialog'}
      labelledBy={titleId}
      describedBy={messageId}
      onClose={onCancel}
      closeOnEscape={!busy}
      closeOnBackdropClick={!busy}
      data-testid={testId}
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
        {secondaryAction && (
          <button
            className="btn-secondary"
            onClick={secondaryAction.onSelect}
            disabled={busy}
            data-testid={secondaryAction.testId}
          >
            {secondaryAction.label}
          </button>
        )}
        <button
          className={danger ? 'btn-primary confirm-dialog-danger' : 'btn-primary'}
          onClick={onConfirm}
          disabled={busy}
          aria-busy={busy || undefined}
          data-testid={confirmTestId ?? 'confirm-dialog-confirm'}
        >
          {busy ? busyLabel : confirmLabel}
        </button>
      </div>
    </Modal>
  )
}
