import { useState } from 'react'
import type { FileDiff } from './chat/events'

/** Collapsible unified-diff card for a `file-diff` event: collapsed it shows
 *  the path and +/− counts; expanded, the diff body with hunk coloring.
 *  Rendered under the edit/write tool card it belongs to, or standalone when
 *  the matching card isn't in the transcript. */
export default function DiffBlock({ diff }: { diff: FileDiff }) {
  const [expanded, setExpanded] = useState(false)
  const lines = diff.diff === '' ? [] : diff.diff.split('\n')
  const hasBody = lines.length > 0

  return (
    <div className="diff-block" data-testid="diff-block">
      <button
        type="button"
        className="diff-header"
        onClick={() => hasBody && setExpanded((v) => !v)}
      >
        <span
          className={`tool-chevron ${expanded ? 'open' : ''} ${hasBody ? '' : 'tool-chevron-leaf'}`}
          aria-hidden="true"
        >
          &#9654;
        </span>
        <span className="diff-title">{diff.created ? 'New file' : 'Diff'}</span>
        <span className="diff-path" title={diff.path}>
          {diff.path}
        </span>
        {diff.added > 0 && <span className="diff-added">+{diff.added}</span>}
        {diff.removed > 0 && <span className="diff-removed">&minus;{diff.removed}</span>}
      </button>
      {expanded && hasBody && (
        <pre className="diff-body">
          {lines.map((l, i) => {
            const cls = l.startsWith('+')
              ? 'diff-line-add'
              : l.startsWith('-')
                ? 'diff-line-del'
                : l.startsWith('@@')
                  ? 'diff-line-hunk'
                  : 'diff-line'
            return (
              <div key={i} className={cls}>
                {l || ' '}
              </div>
            )
          })}
          {diff.truncated && <div className="diff-line-hunk">… diff truncated</div>}
        </pre>
      )}
    </div>
  )
}
