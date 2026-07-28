import type { ReactNode } from 'react'

import Modal from '../Modal'
import './Review.css'

interface Props {
  /** Names the sheet, both visually and for assistive tech. */
  title: string
  testId?: string
  onClose: () => void
  children: ReactNode
}

/**
 * The mobile bottom sheet the review panels (annotations, chat, history)
 * open in.
 *
 * It is `Modal` with a different shape, not a second overlay primitive:
 * the portal to `body` (iOS WebKit clips a fixed element inside a scroll
 * container — the review screen is nothing but scroll containers), the
 * backdrop, Escape / backdrop-tap dismissal and the `useDialogFocus`
 * trap + restore all come from there. Everything below is a header and a
 * grip; the sheet geometry lives in `styles/mobile.css`.
 */
export default function ReviewSheet({ title, testId, onClose, children }: Props) {
  return (
    <Modal
      onClose={onClose}
      className="review-sheet"
      backdropClassName="review-sheet__backdrop"
      data-testid={testId}
    >
      {/* Affordance only — the sheet is dismissed by the close button, the
          backdrop or Escape. A drag would fight the panel's own scroll. */}
      <span className="review-sheet__grip" aria-hidden="true" />
      <header className="review-sheet__head">
        <h2 className="review-sheet__title">{title}</h2>
        <button
          type="button"
          className="review-sheet__close"
          onClick={onClose}
          aria-label={`Close ${title}`}
          data-testid="review-sheet-close"
        >
          <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
            <path
              d="M4 4l8 8M12 4l-8 8"
              stroke="currentColor"
              strokeWidth="1.6"
              strokeLinecap="round"
            />
          </svg>
        </button>
      </header>
      {children}
    </Modal>
  )
}
