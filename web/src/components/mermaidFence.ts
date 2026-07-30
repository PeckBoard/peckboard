/**
 * hast helpers for spotting a fenced ```mermaid block, shared by every
 * markdown surface that renders diagrams (chat, plans, document review).
 *
 * They read the parsed node react-markdown hands each component rather than
 * the rendered children: `rehype-highlight` runs on the same tree, and once a
 * fence has been split into token spans there is no single text child left to
 * read the source off.
 */

interface HastNode {
  type?: string
  tagName?: string
  value?: string
  properties?: { className?: unknown }
  children?: HastNode[]
}

/** The element's class list, however rehype left it (array or string). */
function classList(node: HastNode): string[] {
  const cls = node.properties?.className
  if (Array.isArray(cls)) return cls.map(String)
  if (typeof cls === 'string') return cls.split(/\s+/)
  return []
}

/** True for a `<code>` element fenced as mermaid. Inline code carries no
 *  language class, so it never matches. */
export function isMermaidCode(node: unknown): boolean {
  const el = node as HastNode | undefined
  if (!el || el.tagName !== 'code') return false
  return classList(el).includes('language-mermaid')
}

/** Every text node under `node`, concatenated — the fence's own source. */
function nodeText(node: unknown): string {
  const el = node as HastNode | undefined
  if (!el) return ''
  if (el.type === 'text') return el.value ?? ''
  return (el.children ?? []).map(nodeText).join('')
}

/** The mermaid source ready to hand to mermaid itself: text with the fence's
 *  trailing newline trimmed. */
export function mermaidSource(codeNode: unknown): string {
  return nodeText(codeNode).replace(/\n$/, '')
}

/** The mermaid source inside a `<pre>`, or null when the block is anything
 *  else. For surfaces that override `pre` instead of `code` — the review
 *  pane anchors annotations on the `pre`, which is the node that carries the
 *  block's source position. */
export function mermaidFence(preNode: unknown): string | null {
  const el = preNode as HastNode | undefined
  const code = el?.children?.find((c) => c.type === 'element')
  if (!isMermaidCode(code)) return null
  return mermaidSource(code)
}
