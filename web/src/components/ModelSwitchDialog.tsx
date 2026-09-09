import ConfirmDialog from './ConfirmDialog'
import type { RecoveryPreview } from '../types/api'
import { fmtInt, fmtTokens, fmtUsd } from '../util/format'

export type ModelSwitchMode = 'handover' | 'recovery' | 'clear' | 'force'

interface ModelSwitchDialogProps {
  targetLabel: string
  switchMode: ModelSwitchMode
  setSwitchMode: (mode: ModelSwitchMode) => void
  switchBusy: boolean
  switchError: string | null
  isWorker: boolean
  recoveryPreview: RecoveryPreview | null
  recoveryPreviewError: string | null
  onConfirm: () => void
  onCancel: () => void
}

export default function ModelSwitchDialog({
  targetLabel,
  switchMode,
  setSwitchMode,
  switchBusy,
  switchError,
  isWorker,
  recoveryPreview,
  recoveryPreviewError,
  onConfirm,
  onCancel,
}: ModelSwitchDialogProps) {
  const confirmLabel =
    switchMode === 'recovery'
      ? 'Send transcript'
      : switchMode === 'clear'
        ? 'Clear & switch'
        : switchMode === 'force'
          ? 'Force switch'
          : 'Hand over context'
  const confirmTestId =
    switchMode === 'recovery'
      ? 'model-switch-recovery-confirm'
      : switchMode === 'clear'
        ? 'model-switch-clear-confirm'
        : switchMode === 'force'
          ? 'model-switch-force-confirm'
          : 'model-switch-handover'

  return (
    <ConfirmDialog
      title="Switch model?"
      message={`Switching to ${targetLabel} crosses a provider or account boundary — the new model starts with no memory of this conversation.`}
      wide
      cancelLabel="Cancel"
      confirmLabel={confirmLabel}
      confirmTestId={confirmTestId}
      confirmDisabled={switchMode === 'recovery' && recoveryPreview?.fits === false}
      danger={switchMode === 'recovery' || switchMode === 'clear'}
      busy={switchBusy}
      error={switchError}
      testId="model-switch-prompt"
      onConfirm={onConfirm}
      onCancel={onCancel}
    >
      <div className="confirm-dialog-choices" role="radiogroup" aria-label="How to pass context">
        <label className="confirm-dialog-choice">
          <input
            type="radio"
            name="model-switch-mode"
            checked={switchMode === 'handover'}
            disabled={switchBusy || isWorker}
            onChange={() => setSwitchMode('handover')}
          />
          <span className="confirm-dialog-choice-body">
            <span className="confirm-dialog-choice-label">Hand over a summary</span>
            <span className="confirm-dialog-choice-hint">
              The current agent writes a handover doc. Won&apos;t work if this account has hit a
              usage limit{isWorker ? ' — not available on workers' : ''}.
            </span>
          </span>
        </label>
        <label className="confirm-dialog-choice" data-testid="model-switch-recovery">
          <input
            type="radio"
            name="model-switch-mode"
            checked={switchMode === 'recovery'}
            disabled={switchBusy || isWorker}
            onChange={() => setSwitchMode('recovery')}
          />
          <span className="confirm-dialog-choice-body">
            <span className="confirm-dialog-choice-label">Send full transcript (recovery)</span>
            <span className="confirm-dialog-choice-hint">
              Does not use the current agent. The entire conversation is sent to the new model as
              one input — billed to the new account. Use this when the current account has hit a
              limit.
            </span>
            {recoveryPreviewError ? (
              <span className="confirm-dialog-cost-warn">{recoveryPreviewError}</span>
            ) : recoveryPreview ? (
              <>
                <span className="confirm-dialog-cost">
                  <span data-testid="model-switch-recovery-tokens">
                    ~{fmtInt(recoveryPreview.tokens)} tokens ({fmtTokens(recoveryPreview.tokens)})
                  </span>
                  <span data-testid="model-switch-recovery-cost">
                    ~{fmtUsd(recoveryPreview.est_cost_usd)} at the new model&apos;s input rate
                  </span>
                </span>
                {!recoveryPreview.fits && (
                  <span className="confirm-dialog-cost-warn">
                    This exceeds the new model&apos;s ~{fmtInt(recoveryPreview.context_window)}
                    -token context window. Compact or recap first.
                  </span>
                )}
              </>
            ) : (
              <span className="confirm-dialog-choice-hint">Estimating token cost…</span>
            )}
          </span>
        </label>
        <label className="confirm-dialog-choice" data-testid="model-switch-force">
          <input
            type="radio"
            name="model-switch-mode"
            checked={switchMode === 'force'}
            disabled={switchBusy}
            onChange={() => setSwitchMode('force')}
          />
          <span className="confirm-dialog-choice-body">
            <span className="confirm-dialog-choice-label">Force switch, keep transcript</span>
            <span className="confirm-dialog-choice-hint">
              Does not call either model. History stays on screen; the new model starts with no
              memory. Use this when the current account is out of credits.
            </span>
          </span>
        </label>
        <label className="confirm-dialog-choice" data-testid="model-switch-clear">
          <input
            type="radio"
            name="model-switch-mode"
            checked={switchMode === 'clear'}
            disabled={switchBusy || isWorker}
            onChange={() => setSwitchMode('clear')}
          />
          <span className="confirm-dialog-choice-body">
            <span className="confirm-dialog-choice-label">Clear context and switch</span>
            <span className="confirm-dialog-choice-hint">
              Start fresh. This conversation is not passed on
              {isWorker ? ' — worker transcripts cannot be cleared' : ''}.
            </span>
          </span>
        </label>
      </div>
    </ConfirmDialog>
  )
}
