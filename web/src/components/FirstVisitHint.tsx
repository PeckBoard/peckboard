import type { ReactNode } from 'react'
import HelpTip from './HelpTip'
import { useAuthStore } from '../store/auth'
import { hintId, useFirstVisitHintsStore } from '../store/firstVisitHints'
import { docsUrl, type HelpTerm } from '../utils/helpTerms'

interface Props {
  /** Stable id of this hint, e.g. `projects`. One dismissal per user. */
  hintKey: string
  /** One plain sentence on what the page is for. */
  children: ReactNode
  /** Shared glossary entry explained by a (?) tip after the sentence. */
  term?: HelpTerm
  /** Docs path under `DOCS_BASE` (or a full URL) for a "Learn more" link. */
  docs?: string
}

/**
 * A one-time, dismissible hint shown at the top of a page the first time a
 * user opens it. Dismissal is remembered per user in localStorage, so it
 * never comes back once closed. Renders nothing until the user is known.
 */
export default function FirstVisitHint({ hintKey, children, term, docs }: Props) {
  const userId = useAuthStore((s) => s.user?.id)
  const dismissed = useFirstVisitHintsStore((s) =>
    userId ? s.dismissed.includes(hintId(userId, hintKey)) : true,
  )
  const dismiss = useFirstVisitHintsStore((s) => s.dismiss)
  if (!userId || dismissed) return null
  const href = docs ? docsUrl(docs) : undefined
  return (
    <div className="first-visit-hint" role="note" data-testid={`first-visit-hint-${hintKey}`}>
      <p className="first-visit-hint-text">
        {children}
        {term && <HelpTip term={term} />}
        {href && (
          <a className="form-link" href={href} target="_blank" rel="noreferrer noopener">
            Learn more ↗
          </a>
        )}
      </p>
      <button
        type="button"
        className="first-visit-hint-dismiss"
        aria-label="Dismiss hint"
        data-testid={`first-visit-hint-dismiss-${hintKey}`}
        onClick={() => dismiss(userId, hintKey)}
      >
        Got it
      </button>
    </div>
  )
}
