import { useState } from 'react'
import { authedFetch } from '../store/auth'
import ConfirmDialog from './ConfirmDialog'

/** One provider's entry from `GET /api/settings/provider-prompts`. */
export interface ProviderPrompt {
  provider: string
  label: string
  /** The built-in base prompt (shared working style + provider-specific text). */
  default: string
  /** The user's replacement, or null when the default is in use. */
  override: string | null
}

interface Props {
  entry: ProviderPrompt
  onChange: (entry: ProviderPrompt) => void
}

/**
 * "Base prompt" editor inside a provider's section on Settings → Providers
 * & Accounts. The override REPLACES the provider's whole default prompt;
 * session / card prompts and caveman mode still layer on top. Remount with
 * a `key` derived from the saved text so the draft resets after a save.
 */
export default function ProviderBasePrompt({ entry, onChange }: Props) {
  const saved = entry.override ?? entry.default
  const [draft, setDraft] = useState(saved)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [confirmReset, setConfirmReset] = useState(false)
  const [resetError, setResetError] = useState<string | null>(null)

  const put = async (text: string | null): Promise<ProviderPrompt> => {
    const res = await authedFetch(
      `/api/settings/provider-prompts/${encodeURIComponent(entry.provider)}`,
      {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ text }),
      },
    )
    if (!res.ok) {
      const data = (await res.json().catch(() => null)) as { error?: unknown } | null
      throw new Error(typeof data?.error === 'string' ? data.error : `HTTP ${res.status}`)
    }
    return (await res.json()) as ProviderPrompt
  }

  const save = async () => {
    setBusy(true)
    setError(null)
    try {
      // Saving the default text verbatim is a reset, not an override.
      onChange(await put(draft === entry.default ? null : draft))
    } catch (e) {
      setError(`Failed to save base prompt: ${(e as Error).message}`)
    } finally {
      setBusy(false)
    }
  }

  const reset = async () => {
    setBusy(true)
    setResetError(null)
    try {
      const next = await put(null)
      setConfirmReset(false)
      onChange(next)
    } catch (e) {
      setResetError(`Failed to reset: ${(e as Error).message}`)
    } finally {
      setBusy(false)
    }
  }

  const blank = draft.trim() === ''
  const disabledReason = blank
    ? 'The prompt cannot be empty — use Reset to default instead.'
    : draft === saved
      ? 'No unsaved changes.'
      : null

  return (
    <div className="provider-base-prompt" data-testid={`provider-prompt-${entry.provider}`}>
      <div className="provider-base-prompt-header">
        <h4>Base Prompt</h4>
        {entry.override !== null && (
          <span
            className="plugin-badge plugin-badge--pending"
            data-testid={`provider-prompt-customized-${entry.provider}`}
          >
            Customized
          </span>
        )}
      </div>
      <p className="form-hint">
        The standing system prompt every {entry.label} session starts from. Editing it replaces the
        built-in default; session and card prompts still apply on top.
      </p>
      <textarea
        className="form-input provider-base-prompt-input"
        rows={10}
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        aria-label={`${entry.label} base prompt`}
        data-testid={`provider-prompt-input-${entry.provider}`}
      />
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
      <div className="form-actions">
        {disabledReason && <span className="form-actions-reason">{disabledReason}</span>}
        <button
          type="button"
          className="btn-secondary"
          disabled={busy || entry.override === null}
          onClick={() => {
            setResetError(null)
            setConfirmReset(true)
          }}
          data-testid={`provider-prompt-reset-${entry.provider}`}
        >
          Reset to default
        </button>
        <button
          type="button"
          className="btn-primary"
          disabled={busy || disabledReason !== null}
          onClick={save}
          data-testid={`provider-prompt-save-${entry.provider}`}
        >
          {busy ? 'Saving…' : 'Save'}
        </button>
      </div>
      {confirmReset && (
        <ConfirmDialog
          title="Reset base prompt?"
          message={`Discard your custom ${entry.label} base prompt and restore the built-in default.`}
          confirmLabel="Reset to default"
          danger
          busy={busy}
          busyLabel="Resetting…"
          error={resetError}
          onConfirm={reset}
          onCancel={() => setConfirmReset(false)}
        />
      )}
    </div>
  )
}
