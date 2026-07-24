import { useMemo } from 'react'
import { sanitizePluginIconSvg } from '../utils/pluginIcon'

/** Icon for a plugin-contributed rail entry. Renders the plugin's declared
 * inline-SVG icon after strict sanitization (see utils/pluginIcon.ts); when
 * the plugin declares none — or the markup fails sanitization — falls back
 * to the generic placeholder so every entry still gets a glyph. Sized and
 * colored to match the built-in 18px `currentColor` rail icons. */
export function PluginIcon({ icon }: { icon?: string | null }) {
  const safe = useMemo(() => sanitizePluginIconSvg(icon), [icon])
  if (!safe) {
    return (
      <svg
        width="18"
        height="18"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
      >
        <polygon points="5 3 19 12 5 21 5 3" />
      </svg>
    )
  }
  // Safe: `safe` is the serialized output of the allowlist sanitizer above,
  // never the plugin's raw markup.
  return <span className="plugin-icon" dangerouslySetInnerHTML={{ __html: safe }} />
}
