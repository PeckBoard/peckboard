/** Sanitizer for plugin-declared sidebar icons (inline SVG strings from the
 * `/api/plugins` catalog). Plugins are third-party code, so their markup is
 * never trusted: this is a strict allowlist — anything not explicitly listed
 * (elements, attributes, suspicious attribute values) is stripped, and inputs
 * that fail structural checks are rejected outright so the caller falls back
 * to the generic icon. No scripts, event handlers, hyperlinks, external
 * references, styles, or embedded content can survive.
 */

/** Hard cap mirrored server-side in `collect_items` (plugin/manager.rs). */
const MAX_ICON_CHARS = 8192

/** Static shape/grouping elements only — no `use`/`image` (external refs),
 * no `style`/`script`/`foreignObject`/`animate*`, no filters or gradients
 * (rail icons are single-color `currentColor` line glyphs). */
const ALLOWED_TAGS = new Set([
  'svg',
  'g',
  'path',
  'circle',
  'ellipse',
  'rect',
  'line',
  'polyline',
  'polygon',
  'title',
  'desc',
])

/** Geometry + paint attributes. Notably absent: `href`/`xlink:href`, `style`,
 * `class`, `id`, and every `on*` handler. Namespaced attributes carry their
 * prefix in `attr.name` and therefore never match. */
const ALLOWED_ATTRS = new Set([
  'xmlns',
  'viewBox',
  'width',
  'height',
  'd',
  'points',
  'cx',
  'cy',
  'r',
  'rx',
  'ry',
  'x',
  'y',
  'x1',
  'y1',
  'x2',
  'y2',
  'fill',
  'stroke',
  'stroke-width',
  'stroke-linecap',
  'stroke-linejoin',
  'stroke-miterlimit',
  'stroke-dasharray',
  'stroke-dashoffset',
  'fill-rule',
  'clip-rule',
  'opacity',
  'fill-opacity',
  'stroke-opacity',
  'transform',
])

/** Paint values like `fill="url(#x)"` can reference external resources in
 * SVG2; scheme-ish values never belong in geometry/paint attributes. */
const FORBIDDEN_VALUE = /url\s*\(|javascript:|data:/i

/** Depth-first scrub. Returns false when the element itself is disallowed
 * (caller removes the whole subtree). */
function scrub(el: Element): boolean {
  if (!ALLOWED_TAGS.has(el.localName)) return false
  for (const attr of Array.from(el.attributes)) {
    if (!ALLOWED_ATTRS.has(attr.name) || FORBIDDEN_VALUE.test(attr.value)) {
      el.removeAttribute(attr.name)
    }
  }
  for (const child of Array.from(el.childNodes)) {
    if (child.nodeType === Node.ELEMENT_NODE) {
      if (!scrub(child as Element)) el.removeChild(child)
    } else if (child.nodeType !== Node.TEXT_NODE) {
      // Comments, CDATA sections, processing instructions.
      el.removeChild(child)
    }
  }
  return true
}

/** Sanitize a plugin icon. Returns serialized safe SVG markup (root forced to
 * the rail's 18×18, `aria-hidden`), or null when the input is missing,
 * oversized, unparseable, or not rooted at `<svg>` — the caller then renders
 * the generic fallback icon. */
export function sanitizePluginIconSvg(raw: string | null | undefined): string | null {
  if (!raw || raw.length > MAX_ICON_CHARS) return null
  let doc: Document
  try {
    doc = new DOMParser().parseFromString(raw, 'image/svg+xml')
  } catch {
    return null
  }
  // XML parse failures surface as an embedded <parsererror> document.
  if (doc.getElementsByTagName('parsererror').length > 0) return null
  const root = doc.documentElement
  if (!root || root.localName !== 'svg') return null
  if (!scrub(root)) return null
  root.setAttribute('width', '18')
  root.setAttribute('height', '18')
  root.setAttribute('aria-hidden', 'true')
  root.setAttribute('focusable', 'false')
  return new XMLSerializer().serializeToString(root)
}
