/**
 * Heading extraction + stable heading ids for rendered markdown.
 *
 * Two consumers have to agree on the *same* slug for the *same*
 * heading: the table of contents (built from the markdown source) and
 * the rendered headings (given their `id` by `rehypeHeadingIds` while
 * react-markdown walks the hast tree). Both go through `slugify` and
 * the same first-come dedupe counter, walking headings in document
 * order, so `#<slug>` always resolves.
 *
 * Deliberately dependency-free (no `rehype-slug` / `github-slugger`)
 * and HTML-free: `rehypeHeadingIds` only sets a property on existing
 * heading elements, so [[SafeMarkdown]]'s "raw HTML stays escaped"
 * guarantee is untouched.
 */

export interface TocEntry {
  /** Heading level — 2 or 3; deeper headings are too noisy for a TOC. */
  depth: 2 | 3
  /** Plain-text heading label (inline markdown stripped). */
  text: string
  /** URL fragment, matching the rendered heading's `id`. */
  slug: string
}

const HEADING_TAGS = new Set(['h1', 'h2', 'h3', 'h4', 'h5', 'h6'])

/** GitHub-ish slug: lowercase, punctuation dropped, spaces to dashes. */
export function slugify(text: string): string {
  const slug = text
    .trim()
    .toLowerCase()
    .replace(/[^\p{L}\p{N}\s-]/gu, '')
    .replace(/\s+/g, '-')
    .replace(/-+/g, '-')
    .replace(/^-|-$/g, '')
  return slug || 'section'
}

/** First occurrence keeps the bare slug; later ones get `-1`, `-2`, … */
function dedupe(seen: Map<string, number>, base: string): string {
  const n = seen.get(base) ?? 0
  seen.set(base, n + 1)
  return n === 0 ? base : `${base}-${n}`
}

/** Strip the inline markdown a heading can carry so the TOC label and
 *  the rendered heading's text node both reduce to the same string. */
function plainText(md: string): string {
  return md
    .replace(/!\[[^\]]*\]\([^)]*\)/g, '')
    .replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/`([^`]*)`/g, '$1')
    .replace(/\*\*([^*]+)\*\*/g, '$1')
    .replace(/__([^_]+)__/g, '$1')
    .replace(/~~([^~]+)~~/g, '$1')
    .replace(/([*_])([^*_]+)\1/g, '$2')
    .replace(/\s+/g, ' ')
    .trim()
}

/**
 * Pull the h2/h3 headings out of a markdown document, in order.
 *
 * Fenced code blocks are skipped so a `# comment` inside a shell
 * snippet never lands in the TOC. All heading levels feed the dedupe
 * counter (only h2/h3 are returned) so the slugs stay in lockstep with
 * `rehypeHeadingIds`, which ids every heading it meets.
 */
export function extractToc(markdown: string): TocEntry[] {
  const entries: TocEntry[] = []
  const seen = new Map<string, number>()
  let fence: string | null = null

  for (const line of markdown.split('\n')) {
    const fenceMatch = /^\s{0,3}(`{3,}|~{3,})/.exec(line)
    if (fenceMatch) {
      const marker = fenceMatch[1][0]
      if (fence === null) fence = marker
      else if (fence === marker) fence = null
      continue
    }
    if (fence !== null) continue

    const heading = /^(#{1,6})\s+(.*?)\s*#*\s*$/.exec(line)
    if (!heading) continue
    const depth = heading[1].length
    const text = plainText(heading[2])
    if (!text) continue
    const slug = dedupe(seen, slugify(text))
    if (depth === 2 || depth === 3) entries.push({ depth, text, slug })
  }

  return entries
}

/** Minimal structural view of a hast node — enough to walk the tree and
 *  set a property, without pulling in `@types/hast` or a visit helper. */
interface HastNodeLike {
  type: string
  tagName?: string
  properties?: Record<string, unknown>
  children?: HastNodeLike[]
  value?: string
}

function hastText(node: HastNodeLike): string {
  if (node.type === 'text') return node.value ?? ''
  return (node.children ?? []).map(hastText).join('')
}

/**
 * Rehype plugin giving every h1–h6 a stable `id` so headings can be
 * linked to directly (`/reports/<folder>/<file>#<slug>`).
 *
 * Runs on the hast tree react-markdown already produced — it adds no
 * new nodes and parses no HTML.
 */
export function rehypeHeadingIds() {
  return (tree: HastNodeLike) => {
    const seen = new Map<string, number>()
    const walk = (node: HastNodeLike) => {
      if (node.type === 'element' && node.tagName && HEADING_TAGS.has(node.tagName)) {
        const text = plainText(hastText(node))
        if (text) {
          const slug = dedupe(seen, slugify(text))
          node.properties = { ...(node.properties ?? {}), id: slug }
        }
        return
      }
      for (const child of node.children ?? []) walk(child)
    }
    walk(tree)
  }
}
