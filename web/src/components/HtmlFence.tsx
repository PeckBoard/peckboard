import type { ReactNode } from 'react'

import { openHtmlMockup } from '../store/mockupPane'
import './MockupPane.css'

/** Longer blocks start with the source folded: a whole mockup would otherwise
 *  fill the chat with markup, while a short snippet is there to be read. */
const MAX_OPEN_SOURCE_LINES = 12

/**
 * A fenced ```html block: an Open preview button plus the source. Rendered
 * inside the markdown's `<pre>`. `html` is the block as it stands at the click,
 * so opening one that is still streaming shows what has arrived so far.
 */
export default function HtmlFence({
  html,
  className,
  children,
}: {
  html: string
  className?: string
  children: ReactNode
}) {
  const lines = html.split('\n').length
  return (
    <>
      <span className="html-fence-bar">
        <button
          type="button"
          className="btn-secondary"
          onClick={() => openHtmlMockup(html)}
          data-testid="html-fence-open"
        >
          Open preview
        </button>
      </span>
      <details className="html-fence-source" open={lines <= MAX_OPEN_SOURCE_LINES}>
        <summary>Source ({lines === 1 ? '1 line' : `${lines} lines`})</summary>
        <code className={className}>{children}</code>
      </details>
    </>
  )
}
