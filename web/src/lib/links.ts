/** Most link previews rendered under one message. */
export const MAX_PREVIEWS_PER_MESSAGE = 3

const URL_RE = /\bhttps?:\/\/[^\s<>"'`()[\]{}]+/gi
/** Punctuation that ends a sentence rather than the URL. */
const TRAILING = /[.,;:!?*_~'"]+$/

/** Markdown text with fenced blocks and inline code spans blanked, so a URL
 *  inside code never becomes a preview. */
function withoutCode(text: string): string {
  return text
    .replace(/(^|\n)[ \t]*(```|~~~)[^\n]*\n[\s\S]*?(?:\n[ \t]*\2[^\n]*(?=\n|$)|$)/g, '$1')
    .replace(/`[^`\n]*`/g, ' ')
}

/** The distinct http(s) links in a chat message, in order, at most `max`.
 *  Links inside code blocks / spans are skipped. */
export function extractLinks(text: string, max = MAX_PREVIEWS_PER_MESSAGE): string[] {
  const out: string[] = []
  const seen = new Set<string>()
  for (const m of withoutCode(text).matchAll(URL_RE)) {
    const raw = m[0].replace(TRAILING, '')
    let url: URL
    try {
      url = new URL(raw)
    } catch {
      continue
    }
    if (!url.hostname) continue
    const key = url.href.replace(/#.*$/, '')
    if (seen.has(key)) continue
    seen.add(key)
    out.push(url.href)
    if (out.length >= max) break
  }
  return out
}
