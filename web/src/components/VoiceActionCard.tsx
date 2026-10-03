import { useState } from 'react'
import { useVoiceStore, type VoiceAction } from '../store/voice'

/**
 * A gated tool call the voice assistant asked for, waiting on the user.
 * The server parked the exact call; only Confirm here (or a spoken "yes",
 * which presses the same route) runs it — nothing the assistant says can.
 * Inline in the panel with the `ConfirmDialog` skin, so it never blocks
 * the conversation.
 */
export default function VoiceActionCard({ action }: { action: VoiceAction }) {
  const confirmAction = useVoiceStore((s) => s.confirmAction)
  const cancelAction = useVoiceStore((s) => s.cancelAction)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const run = async (fn: (id: string) => Promise<string | null>) => {
    setBusy(true)
    setError(null)
    const err = await fn(action.id)
    setBusy(false)
    if (err) setError(err)
  }

  return (
    <div
      className="voice-action"
      role="alertdialog"
      aria-label="Confirm the assistant's action"
      data-testid="voice-action"
      data-action-id={action.id}
    >
      <h3 className="confirm-dialog-title">Confirm action</h3>
      <p className="confirm-dialog-message" data-testid="voice-action-summary">
        {action.summary}
      </p>
      {error && (
        <p className="confirm-dialog-error" role="alert" data-testid="voice-action-error">
          {error}
        </p>
      )}
      <div className="confirm-dialog-actions">
        <button
          type="button"
          className="btn-secondary"
          disabled={busy}
          onClick={() => void run(cancelAction)}
          data-testid="voice-action-cancel"
        >
          Cancel
        </button>
        <button
          type="button"
          className="btn-primary confirm-dialog-danger"
          disabled={busy}
          aria-busy={busy || undefined}
          onClick={() => void run(confirmAction)}
          data-testid="voice-action-confirm"
        >
          {busy ? 'Working…' : 'Confirm'}
        </button>
      </div>
    </div>
  )
}
