import { useEffect, useMemo, useRef, type ElementType, type ReactNode } from 'react'
import type { Components } from 'react-markdown'

import SafeMarkdown from '../SafeMarkdown'
import { isOpenComment, type DocReviewComment } from '../../lib/review'
import './Review.css'

/** What the user pointed at: the source line range to anchor an annotation
 *  to, plus the text itself (a display aid once a revision moves the lines). */
export interface BlockAnchor {
  startLine: number
  endLine: number
  quote: string
}

interface Props {
  markdown: string
  /** Every annotation on the review. Open ones tint their block; resolved
   *  ones keep a fainter mark so a revised passage stays traceable. */
  comments: DocReviewComment[]
  /** The annotation the rail (or a pin) has focused — its block is
   *  highlighted and scrolled into view. */
  activeCommentId: string | null
  onAnchor: (anchor: BlockAnchor, at: { x: number; y: number }) => void
  onSelectComment: (commentId: string) => void
}

/** Block-level tags that become annotation anchors. Inline elements are
 *  deliberately absent: an annotation on half a word has nothing useful to
 *  say to the assistant, and every anchor is a click target the user has to
 *  aim past. */
const BLOCK_TAGS = ['p', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'li', 'pre', 'blockquote'] as const

/** The stored quote is a display aid, not the anchor — a whole-section
 *  selection doesn't need to travel over the wire intact. */
const MAX_QUOTE = 2_000

interface BlockProps {
  node?: unknown
  children?: ReactNode
  className?: string
}

interface LineRange {
  start: number
  end: number
}

/** mdast keeps 1-based source positions on every parsed node, and
 *  react-markdown forwards them as `node.position`. A node a plugin
 *  synthesised has none — it renders normally and simply isn't annotatable,
 *  rather than anchoring an annotation to line NaN. */
function positionOf(node: unknown): LineRange | null {
  const pos = (node as { position?: { start?: { line?: number }; end?: { line?: number } } })
    ?.position
  const start = pos?.start?.line
  const end = pos?.end?.line
  if (typeof start !== 'number' || typeof end !== 'number' || start < 1) return null
  return { start, end: Math.max(start, end) }
}

/** The tint a block wears, in precedence order: anything still open reads as
 *  pending, otherwise the block shows how its annotations were closed out. */
function toneFor(hits: DocReviewComment[]): string | null {
  if (hits.length === 0) return null
  if (hits.some(isOpenComment)) return 'review-block--pending'
  if (hits.some((c) => c.status === 'fixed')) return 'review-block--fixed'
  if (hits.some((c) => c.status === 'declined')) return 'review-block--declined'
  return 'review-block--answered'
}

function prefersReducedMotion(): boolean {
  return window.matchMedia?.('(prefers-reduced-motion: reduce)').matches ?? false
}

/**
 * The rendered document, with every block anchored back to its source lines.
 *
 * `SafeMarkdown` renders it — no `rehype-raw`, no relaxed URL transform, the
 * security contract is untouched. All this adds is a `components` map that
 * wraps the block-level tags with `data-line-start` / `data-line-end` read
 * off the parsed node, so a click or a text selection resolves to the exact
 * range the annotation should hang off.
 */
export default function DocPane({
  markdown,
  comments,
  activeCommentId,
  onAnchor,
  onSelectComment,
}: Props) {
  const rootRef = useRef<HTMLDivElement | null>(null)

  const components = useMemo<Components>(() => {
    const renderBlock = (tag: string, { node, children, className }: BlockProps) => {
      const range = positionOf(node)
      const hits = range
        ? comments.filter((c) => c.start_line <= range.end && c.end_line >= range.start)
        : []
      const active = hits.some((c) => c.id === activeCommentId)
      const attrs = {
        className: [className, 'review-block', toneFor(hits), active && 'review-block--active']
          .filter(Boolean)
          .join(' '),
        'data-testid': 'review-block',
        'data-line-start': range?.start,
        'data-line-end': range?.end,
        // Focusable so the popover is reachable without a pointer; the
        // container turns Enter/Space into the same open as a click.
        tabIndex: range ? 0 : undefined,
      }
      const pin =
        hits.length > 0 ? (
          <button
            type="button"
            className="review-block__pin"
            data-testid="review-block-pin"
            aria-label={`${hits.length} annotation${hits.length === 1 ? '' : 's'} on this passage`}
            onMouseUp={(e) => e.stopPropagation()}
            onClick={(e) => {
              e.stopPropagation()
              onSelectComment(hits[0].id)
            }}
          >
            {hits.length}
          </button>
        ) : null

      // A <button> is not valid inside <table>, so the table anchors on a
      // wrapper instead of the element itself — same attributes either way.
      // The wrapper shrinks to the table so its hover wash and tint don't
      // run the full column width past the last cell.
      if (tag === 'table') {
        return (
          <div {...attrs} className={`${attrs.className} review-block--table`}>
            {pin}
            <table>{children}</table>
          </div>
        )
      }
      const Tag = tag as ElementType
      return (
        <Tag {...attrs}>
          {pin}
          {children}
        </Tag>
      )
    }

    const map: Record<string, (props: BlockProps) => ReactNode> = {}
    for (const tag of BLOCK_TAGS) map[tag] = (props) => renderBlock(tag, props)
    map.table = (props) => renderBlock('table', props)
    return map as Components
  }, [comments, activeCommentId, onSelectComment])

  // Follow the rail: focusing an annotation there scrolls its block into the
  // middle of the pane so the two panes always talk about the same passage.
  useEffect(() => {
    if (!activeCommentId) return
    const el = rootRef.current?.querySelector('.review-block--active')
    el?.scrollIntoView({
      block: 'center',
      behavior: prefersReducedMotion() ? 'auto' : 'smooth',
    })
  }, [activeCommentId, markdown])

  /** Open the popover against `el`, quoting `quote`. */
  const openFor = (el: HTMLElement, quote: string, rect: DOMRect) => {
    const start = Number(el.getAttribute('data-line-start'))
    const end = Number(el.getAttribute('data-line-end'))
    if (!Number.isFinite(start) || !Number.isFinite(end) || start < 1) return
    onAnchor(
      { startLine: start, endLine: end, quote: quote.trim().slice(0, MAX_QUOTE) },
      {
        x: rect.left,
        y: rect.bottom + 6,
      },
    )
  }

  // One handler for both gestures. A live selection inside a single block
  // quotes exactly what the user highlighted; anything else (a plain click,
  // a selection spanning blocks) falls back to the whole block.
  const onMouseUp = (e: React.MouseEvent<HTMLDivElement>) => {
    const root = rootRef.current
    if (!root) return
    const selection = window.getSelection()
    if (selection && !selection.isCollapsed && selection.rangeCount > 0) {
      const range = selection.getRangeAt(0)
      const node = range.commonAncestorContainer
      const from = node.nodeType === Node.ELEMENT_NODE ? (node as HTMLElement) : node.parentElement
      const host = from?.closest<HTMLElement>('[data-line-start]') ?? null
      const text = selection.toString()
      if (host && root.contains(host) && text.trim()) {
        openFor(host, text, range.getBoundingClientRect())
        return
      }
    }
    const block = (e.target as HTMLElement | null)?.closest<HTMLElement>('[data-line-start]')
    if (!block || !root.contains(block)) return
    openFor(block, block.textContent ?? '', block.getBoundingClientRect())
  }

  const onKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if (e.key !== 'Enter' && e.key !== ' ') return
    const target = e.target as HTMLElement | null
    // The pin is a real button — let it handle its own activation.
    if (target?.closest('button')) return
    const block = target?.closest<HTMLElement>('[data-line-start]')
    if (!block) return
    e.preventDefault()
    openFor(block, block.textContent ?? '', block.getBoundingClientRect())
  }

  return (
    <div
      ref={rootRef}
      className="review-doc"
      data-testid="review-doc"
      onMouseUp={onMouseUp}
      onKeyDown={onKeyDown}
    >
      <SafeMarkdown className="chat-markdown review-doc__body" components={components}>
        {markdown}
      </SafeMarkdown>
    </div>
  )
}
