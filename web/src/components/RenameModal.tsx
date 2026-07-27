import { useEffect, useId, useRef, useState, type FormEvent } from 'react'
import Modal from './Modal'

interface Props {
  /** Dialog heading, e.g. "Rename session". */
  title: string
  /** Field label, e.g. "Session name". */
  label: string
  /** Prefilled value; selected on open so typing replaces it. */
  initialValue: string
  /** Resolves on success (the modal then closes), rejects to show the
   *  server's message inline while the modal stays open. */
  onSubmit: (name: string) => Promise<void>
  onClose: () => void
}

/**
 * The app's one rename dialog — session, project, repeating task and the
 * chat toolbar all share it. Replaces `window.prompt`, which is unstyled,
 * silently suppressible (making Rename a no-op in iOS standalone/PWA
 * mode) and has nowhere to report a failed rename.
 *
 * Enter submits, Escape / backdrop cancels (via `Modal`), and the field
 * is focused with its text selected on open.
 */
export default function RenameModal({ title, label, initialValue, onSubmit, onClose }: Props) {
  const [value, setValue] = useState(initialValue)
  const [error, setError] = useState('')
  const [busy, setBusy] = useState(false)
  const inputRef = useRef<HTMLInputElement>(null)
  const fieldId = useId()

  // Child effects run before the panel's `useDialogFocus`, which leaves
  // focus alone once it's already inside the panel — so this wins.
  useEffect(() => {
    inputRef.current?.focus()
    inputRef.current?.select()
  }, [])

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    const name = value.trim()
    if (!name) {
      setError('Name cannot be empty')
      inputRef.current?.focus()
      return
    }
    if (name === initialValue) {
      onClose()
      return
    }
    setError('')
    setBusy(true)
    try {
      await onSubmit(name)
      onClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Rename failed')
      setBusy(false)
    }
  }

  return (
    <Modal onClose={onClose} maxWidth={420} data-testid="rename-modal">
      <h2>{title}</h2>
      <form onSubmit={handleSubmit}>
        <div className="form-field">
          <label className="form-label" htmlFor={fieldId}>
            {label}
          </label>
          <input
            id={fieldId}
            ref={inputRef}
            className="form-input"
            type="text"
            value={value}
            onChange={(e) => {
              setValue(e.target.value)
              if (error) setError('')
            }}
            aria-invalid={error ? true : undefined}
            aria-describedby={error ? `${fieldId}-error` : undefined}
            data-testid="rename-input"
          />
          {error && (
            <p className="form-error" id={`${fieldId}-error`} data-testid="rename-error">
              {error}
            </p>
          )}
        </div>
        <div className="form-actions">
          <button type="button" className="btn-secondary" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button type="submit" className="btn-primary" disabled={busy} data-testid="rename-submit">
            {busy ? 'Saving…' : 'Rename'}
          </button>
        </div>
      </form>
    </Modal>
  )
}
