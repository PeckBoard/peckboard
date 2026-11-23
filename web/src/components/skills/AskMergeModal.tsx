import { useState } from 'react'
import Modal from '../Modal'
import FieldError from '../FieldError'
import {
  createMergeRequest,
  findingLabel,
  forkParts,
  GuardError,
  type GuardFinding,
  type MergeRequest,
} from '../../store/skills'

interface Props {
  /** The fork, `name@handle`. */
  fork: string
  onClose: () => void
  onCreated?: (mr: MergeRequest) => void
}

/**
 * "Ask the author to merge": opens a merge request from this install's
 * fork into the original skill. Edits not yet on the fork are published to
 * it first (the content check runs as for any publish).
 */
export default function AskMergeModal({ fork, onClose, onCreated }: Props) {
  const original = forkParts(fork)?.base ?? fork
  const [title, setTitle] = useState('')
  const [description, setDescription] = useState('')
  const [touched, setTouched] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [findings, setFindings] = useState<GuardFinding[]>([])
  const [done, setDone] = useState<{ mr: MergeRequest; published: string | null } | null>(null)
  const titleError = title.trim() ? '' : 'Give it a title saying what you changed.'

  const submit = () => {
    setTouched(true)
    if (titleError) return
    setBusy(true)
    setError(null)
    setFindings([])
    createMergeRequest(fork, title.trim(), description)
      .then((res) => {
        setDone({ mr: res.merge_request, published: res.published_version })
        onCreated?.(res.merge_request)
      })
      .catch((err: unknown) => {
        if (err instanceof GuardError) setFindings(err.findings)
        else setError(err instanceof Error ? err.message : 'Failed to open the merge request')
      })
      .finally(() => setBusy(false))
  }

  return (
    <Modal onClose={busy ? () => {} : onClose} maxWidth={600} data-testid="skill-ask-merge-modal">
      <h2>Ask the author to merge</h2>
      {done ? (
        <div className="skill-publish-result" role="status" data-testid="skill-ask-merge-done">
          <p>Sent. The author of {original} can now review your changes and merge them.</p>
          {done.published && (
            <p className="form-hint">
              Your latest edits were published to the fork first (v{done.published}).
            </p>
          )}
          <p className="form-hint">Follow it under Merge requests → Outgoing.</p>
        </div>
      ) : (
        <>
          <p className="form-hint">
            Sends the changes in <code>{fork}</code> to the author of <code>{original}</code>. They
            can comment, merge them, or close the request.
          </p>
          <div className="form-field">
            <label className="form-label" htmlFor="skill-mr-title">
              Title
            </label>
            <input
              id="skill-mr-title"
              className="form-input"
              value={title}
              maxLength={200}
              disabled={busy}
              onChange={(e) => setTitle(e.target.value)}
              onBlur={() => setTouched(true)}
              data-testid="skill-mr-title"
            />
            {touched && <FieldError message={titleError} testId="skill-mr-title-error" />}
          </div>
          <div className="form-field">
            <label className="form-label" htmlFor="skill-mr-description">
              What changed and why (optional)
            </label>
            <textarea
              id="skill-mr-description"
              className="form-input"
              rows={6}
              value={description}
              disabled={busy}
              onChange={(e) => setDescription(e.target.value)}
              data-testid="skill-mr-description"
            />
            {findings.length > 0 && (
              <div className="skill-guard-findings">
                <FieldError message="Not sent: the fork's text looks like it has proprietary content. Edit it to remove these and try again." />
                {findings.map((f, i) => (
                  <FieldError key={i} message={findingLabel(f)} />
                ))}
              </div>
            )}
          </div>
          {error && (
            <p className="form-error" role="alert" data-testid="skill-mr-error">
              {error}
            </p>
          )}
        </>
      )}
      <div className="form-actions">
        {done ? (
          <button
            type="button"
            className="btn-primary"
            onClick={onClose}
            data-testid="skill-mr-close"
          >
            Close
          </button>
        ) : (
          <>
            {touched && titleError && (
              <span className="form-actions-reason">A title is needed</span>
            )}
            <button type="button" className="btn-secondary" onClick={onClose} disabled={busy}>
              Cancel
            </button>
            <button
              type="button"
              className="btn-primary"
              onClick={submit}
              disabled={busy || (touched && !!titleError)}
              data-testid="skill-mr-submit"
            >
              {busy ? 'Sending…' : 'Send'}
            </button>
          </>
        )}
      </div>
    </Modal>
  )
}
