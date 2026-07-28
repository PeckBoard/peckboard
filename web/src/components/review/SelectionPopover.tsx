import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'

import Modal from '../Modal'
import { useMediaQuery } from '../../hooks/useMediaQuery'
import { describeActionError } from '../../utils/actionError'
import './Review.css'

/** The six things a passage can be asked for. Five become annotations; the
 *  sixth (`clarify`) asks a question without touching the document. */
export type PopoverAction = 'comment' | 'suggest' | 'wrong' | 'clarify' | 'expand' | 'shorten'

interface ActionSpec {
  id: PopoverAction
  label: string
  placeholder: string
  /** An empty body is meaningless for these — "this is wrong" with no note
   *  gives the assistant nothing to act on. Expand/shorten/clarify carry
   *  their whole instruction in the verb, so they submit empty. */
  needsBody: boolean
}

const ACTIONS: ActionSpec[] = [
  {
    id: 'comment',
    label: 'Comment',
    placeholder: 'What should the reviewer know about this passage?',
    needsBody: true,
  },
  {
    id: 'suggest',
    label: 'Suggest edit',
    placeholder: 'The replacement text for this passage',
    needsBody: true,
  },
  { id: 'wrong', label: 'Mark wrong', placeholder: "What's wrong here?", needsBody: true },
  {
    id: 'clarify',
    label: 'Clarify',
    placeholder: 'Optional: what would you like explained?',
    needsBody: false,
  },
  {
    id: 'expand',
    label: 'Expand',
    placeholder: 'Optional: what should be added?',
    needsBody: false,
  },
  {
    id: 'shorten',
    label: 'Shorten',
    placeholder: 'Optional: what should go?',
    needsBody: false,
  },
]

interface Props {
  /** Viewport point the popover hangs off — the bottom-left of the selection
   *  rect or of the clicked block. */
  anchor: { x: number; y: number }
  quote: string
  onClose: () => void
  /** Resolves once the annotation is stored (or the clarify pass is away).
   *  Rejecting keeps the popover open with the reason. */
  onSubmit: (action: PopoverAction, body: string) => Promise<void>
}

const MARGIN = 8

/**
 * The action popover for a selected passage: six verbs, then an inline
 * editor for the one that was picked.
 *
 * Portalled to the body and viewport-clamped the same way `Dropdown` is —
 * iOS WebKit clips a `position: fixed` child of a scroll container, which
 * would put the popover under the document pane on exactly the devices that
 * have the least room for it.
 */
export default function SelectionPopover({ anchor, quote, onClose, onSubmit }: Props) {
  const isMobile = useMediaQuery('(max-width: 768px)')
  const ref = useRef<HTMLDivElement | null>(null)
  const firstActionRef = useRef<HTMLButtonElement | null>(null)
  const textareaRef = useRef<HTMLTextAreaElement | null>(null)
  const [action, setAction] = useState<ActionSpec | null>(null)
  const [draft, setDraft] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [pos, setPos] = useState({ left: anchor.x, top: anchor.y })

  // Clamp into the viewport once the popup has a measured size. `pos` is
  // deliberately out of the deps and updated functionally: a shrink-to-fit
  // fixed element reports a fractionally different width after every
  // reposition, and feeding that back in oscillates forever.
  useLayoutEffect(() => {
    const el = ref.current
    if (!el) return
    const rect = el.getBoundingClientRect()
    let left = anchor.x
    let top = anchor.y
    if (left + rect.width > window.innerWidth - MARGIN)
      left = window.innerWidth - rect.width - MARGIN
    if (left < MARGIN) left = MARGIN
    if (top + rect.height > window.innerHeight - MARGIN) {
      top = Math.max(MARGIN, window.innerHeight - rect.height - MARGIN)
    }
    setPos((p) => (p.left === left && p.top === top ? p : { left, top }))
  }, [anchor.x, anchor.y, action])

  // Escape closes the popover — including from inside the editor, where the
  // draft is cheap to retype and being stuck in a modal-ish box is not.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return
      e.stopPropagation()
      e.preventDefault()
      onClose()
    }
    const onDown = (e: MouseEvent) => {
      if ((e.target as HTMLElement | null)?.closest('.review-popover')) return
      onClose()
    }
    document.addEventListener('keydown', onKey, true)
    document.addEventListener('mousedown', onDown)
    return () => {
      document.removeEventListener('keydown', onKey, true)
      document.removeEventListener('mousedown', onDown)
    }
  }, [onClose])

  // Hand focus back where it came from. The desktop popover is a portalled
  // `role="dialog"`, so without this Escape (or a click outside) drops focus
  // on `<body>` and a keyboard user restarts their tab order at the top of
  // the page instead of on the passage they were annotating. `Modal` already
  // does this for the mobile sheet.
  useEffect(() => {
    const opener = document.activeElement as HTMLElement | null
    return () => {
      // React runs this cleanup while the popover's DOM is still attached and
      // still holds focus; detaching it a moment later sends focus to
      // `<body>` anyway. Restore on the next frame instead, and only if
      // nothing else has claimed focus in the meantime.
      requestAnimationFrame(() => {
        if (document.activeElement && document.activeElement !== document.body) return
        if (opener && opener !== document.body && document.contains(opener)) opener.focus()
      })
    }
  }, [])

  // Focus follows the step: the verbs first, then the box you type in.
  useEffect(() => {
    if (action) textareaRef.current?.focus()
    else firstActionRef.current?.focus()
  }, [action])

  const submit = () => {
    if (!action || busy) return
    const body = draft.trim()
    if (action.needsBody && !body) {
      setError('Say what you want changed.')
      return
    }
    setBusy(true)
    setError(null)
    onSubmit(action.id, body)
      .then(() => onClose())
      .catch((e: unknown) => {
        setError(describeActionError(e, "Couldn't save that. Please try again."))
        setBusy(false)
      })
  }

  const body = (
    <>
      {quote && <p className="review-popover__quote">{quote}</p>}

      {!action ? (
        <div className="review-popover__actions">
          {ACTIONS.map((a, i) => (
            <button
              key={a.id}
              ref={i === 0 ? firstActionRef : undefined}
              type="button"
              className="review-popover__action"
              data-testid={`review-popover-${a.id}`}
              onClick={() => {
                setError(null)
                setAction(a)
              }}
            >
              {a.label}
            </button>
          ))}
        </div>
      ) : (
        <div className="review-popover__editor">
          <span className="review-popover__editor-label">{action.label}</span>
          <textarea
            ref={textareaRef}
            className="form-input review-popover__textarea"
            data-testid="review-annotation-editor"
            rows={3}
            value={draft}
            placeholder={action.placeholder}
            aria-label={action.placeholder}
            disabled={busy}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
                e.preventDefault()
                submit()
              }
            }}
          />
          {error && (
            <p className="form-error review-popover__error" role="alert">
              {error}
            </p>
          )}
          <div className="review-popover__editor-actions">
            <button
              type="button"
              className="btn-secondary"
              disabled={busy}
              onClick={() => {
                setAction(null)
                setError(null)
              }}
            >
              Back
            </button>
            <button
              type="button"
              className="btn-primary"
              data-testid="review-annotation-submit"
              disabled={busy}
              aria-busy={busy || undefined}
              onClick={submit}
            >
              {busy ? 'Saving…' : action.id === 'clarify' ? 'Ask' : 'Add'}
            </button>
          </div>
        </div>
      )}
    </>
  )

  // Mobile: a bottom sheet through the shared Modal. A 300px popover hung
  // off a block is unusable at 390px, the verbs need 44px tap targets, and
  // the editor's textarea wants the keyboard-aware height Modal's backdrop
  // already carries.
  if (isMobile) {
    return (
      <Modal
        onClose={onClose}
        className="review-popover review-popover--sheet"
        backdropClassName="review-sheet__backdrop"
        ariaLabel="Annotate this passage"
        data-testid="review-popover"
      >
        <span className="review-sheet__grip" aria-hidden="true" />
        {body}
      </Modal>
    )
  }

  return createPortal(
    <div
      ref={ref}
      className="review-popover"
      data-testid="review-popover"
      role="dialog"
      aria-label="Annotate this passage"
      style={{ position: 'fixed', left: pos.left, top: pos.top }}
    >
      {body}
    </div>,
    document.body,
  )
}
