import { useEffect, useId, useLayoutEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { docsUrl, HELP_TERMS, type HelpTerm } from '../utils/helpTerms'

interface Props {
  /** A shared glossary entry. Pass `title` and `body` instead for one-off text. */
  term?: HelpTerm
  title?: string
  body?: string
  /** Docs path under `DOCS_BASE`, or a full URL. */
  docs?: string
  testId?: string
}

const GAP = 6
const MAX_WIDTH = 280

/**
 * A small (?) button that opens a short, plain explanation of a term, with an
 * optional "Learn more" link. Put it next to a label or heading whose word a
 * new user may not know. Click or Enter opens it; Escape, a second click or a
 * click elsewhere closes it. Portal-rendered with fixed positioning so a
 * modal's overflow can't clip it.
 */
export default function HelpTip({ term, title, body, docs, testId }: Props) {
  const entry = term ? HELP_TERMS[term] : undefined
  const heading = title ?? entry?.title ?? ''
  const text = body ?? entry?.body ?? ''
  const link = docs ?? entry?.docs
  const href = link ? docsUrl(link) : undefined

  const [open, setOpen] = useState(false)
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null)
  const buttonRef = useRef<HTMLButtonElement>(null)
  const popRef = useRef<HTMLDivElement>(null)
  const popId = useId()

  useLayoutEffect(() => {
    if (!open || !buttonRef.current) return
    const r = buttonRef.current.getBoundingClientRect()
    const width = Math.min(MAX_WIDTH, window.innerWidth - GAP * 2)
    const left = Math.max(GAP, Math.min(r.left, window.innerWidth - width - GAP))
    setPos({ left, top: r.bottom + GAP })
  }, [open])

  useEffect(() => {
    if (!open) return
    const onDown = (e: MouseEvent) => {
      const t = e.target as Node
      if (buttonRef.current?.contains(t) || popRef.current?.contains(t)) return
      setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return
      // Close the tip only, not the modal it may sit in.
      e.stopPropagation()
      setOpen(false)
      buttonRef.current?.focus()
    }
    document.addEventListener('mousedown', onDown)
    document.addEventListener('keydown', onKey, true)
    return () => {
      document.removeEventListener('mousedown', onDown)
      document.removeEventListener('keydown', onKey, true)
    }
  }, [open])

  if (!text) return null

  return (
    <>
      <button
        ref={buttonRef}
        type="button"
        className="help-tip-button"
        aria-label={`What is ${heading || 'this'}?`}
        aria-expanded={open}
        aria-controls={open ? popId : undefined}
        onClick={(e) => {
          // A tip inside a <label> must not toggle the labelled control.
          e.preventDefault()
          e.stopPropagation()
          setOpen((o) => !o)
        }}
        data-testid={testId ?? (term ? `help-tip-${term}` : undefined)}
      >
        ?
      </button>
      {open &&
        pos &&
        createPortal(
          <div
            ref={popRef}
            id={popId}
            role="dialog"
            aria-label={heading || 'Help'}
            className="help-tip-popover"
            style={{ position: 'fixed', left: pos.left, top: pos.top, maxWidth: MAX_WIDTH }}
            data-testid="help-tip-popover"
          >
            {heading && <strong className="help-tip-title">{heading}</strong>}
            <p className="help-tip-body">{text}</p>
            {href && (
              <a className="form-link" href={href} target="_blank" rel="noreferrer noopener">
                Learn more ↗
              </a>
            )}
          </div>,
          document.body,
        )}
    </>
  )
}
