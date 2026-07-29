import { useState } from 'react'

import Modal from '../Modal'
import FieldError from '../FieldError'
import { describeActionError } from '../../utils/actionError'
import { linkPr, type DocReviewPrLink, type PrSuggestion } from '../../lib/review'
import './Review.css'

interface Props {
  reviewId: string
  /** What the file's checkout points at, when it is a GitHub one. */
  suggestion: PrSuggestion | null
  onClose: () => void
  onLinked: (link: DocReviewPrLink) => void
}

/**
 * Tie a file review to the pull request that changes that file.
 *
 * The common case is one click: the file sits in a checkout, the checkout
 * has a GitHub remote, and its branch has an open PR — so the whole thing is
 * already known and the dialog just asks you to confirm it. The fields are
 * there for everything else (a PR from another branch, a fork, a number you
 * know and we don't) and start prefilled from whatever was detected.
 */
export default function PrLinkDialog({ reviewId, suggestion, onClose, onLinked }: Props) {
  const [owner, setOwner] = useState(suggestion?.owner ?? '')
  const [repo, setRepo] = useState(suggestion?.repo ?? '')
  const [number, setNumber] = useState(suggestion?.number ? String(suggestion.number) : '')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const submit = async (input: { owner: string; repo: string; number: number }) => {
    setBusy(true)
    setError(null)
    try {
      onLinked(await linkPr(reviewId, { ...input, file_path: suggestion?.file_path }))
    } catch (e) {
      setError(describeActionError(e, "Couldn't link that pull request."))
    } finally {
      setBusy(false)
    }
  }

  const parsed = Number(number.trim())
  const disabled = busy || !owner.trim() || !repo.trim() || !Number.isInteger(parsed) || parsed < 1

  return (
    <Modal onClose={onClose} maxWidth={460} data-testid="review-pr-dialog">
      <h2>Link a pull request</h2>
      <p className="form-hint">
        Line comments on this file in the pull request come in as annotations, and resolving one
        replies on its thread.
      </p>

      {suggestion?.number != null && (
        <button
          type="button"
          className="btn-primary review-pr-dialog__suggested"
          data-testid="review-pr-suggested"
          disabled={busy}
          onClick={() =>
            void submit({
              owner: suggestion.owner,
              repo: suggestion.repo,
              number: suggestion.number as number,
            })
          }
        >
          Link {suggestion.owner}/{suggestion.repo}#{suggestion.number}
        </button>
      )}

      <div className="form-field">
        <label className="form-label" htmlFor="review-pr-owner">
          Owner
        </label>
        <input
          id="review-pr-owner"
          className="form-input"
          data-testid="review-pr-owner"
          value={owner}
          onChange={(e) => setOwner(e.target.value)}
          placeholder="acme"
        />
      </div>
      <div className="form-field">
        <label className="form-label" htmlFor="review-pr-repo">
          Repository
        </label>
        <input
          id="review-pr-repo"
          className="form-input"
          data-testid="review-pr-repo"
          value={repo}
          onChange={(e) => setRepo(e.target.value)}
          placeholder="app"
        />
      </div>
      <div className="form-field">
        <label className="form-label" htmlFor="review-pr-number">
          Pull request number
        </label>
        <input
          id="review-pr-number"
          className="form-input"
          data-testid="review-pr-number"
          inputMode="numeric"
          value={number}
          onChange={(e) => setNumber(e.target.value)}
          placeholder="42"
        />
      </div>
      <FieldError message={error ?? undefined} testId="review-pr-error" />

      <div className="modal-actions">
        <button type="button" className="btn-secondary" onClick={onClose} disabled={busy}>
          Cancel
        </button>
        <button
          type="button"
          className="btn-primary"
          data-testid="review-pr-link"
          disabled={disabled}
          onClick={() => void submit({ owner: owner.trim(), repo: repo.trim(), number: parsed })}
        >
          Link
        </button>
      </div>
    </Modal>
  )
}
