// Drives the `<meta name="theme-color">` tags so the iOS PWA status
// bar matches the nav rail's `--surface`. Lives in its own module so
// both SettingsPage (on user theme change) and App init can share it
// without violating react-refresh's components-only export rule.

export type Theme = 'light' | 'dark' | 'auto'

// Kept in sync with `--surface` in index.css. If --surface changes,
// update both places.
const SURFACE_LIGHT = '#ffffff'
const SURFACE_DARK = '#1a1d27'

// Swap the `theme-color` meta tags so they reflect the user's chosen
// theme. For auto we keep two media-conditioned tags (one per scheme);
// for an explicit light/dark choice we replace them with a single tag
// holding the matching color, since the user's pick can deliberately
// disagree with the OS scheme.
export function applyThemeColor(theme: Theme) {
  const head = document.head
  head.querySelectorAll('meta[name="theme-color"]').forEach((m) => m.remove())
  const make = (content: string, media?: string) => {
    const m = document.createElement('meta')
    m.setAttribute('name', 'theme-color')
    if (media) m.setAttribute('media', media)
    m.setAttribute('content', content)
    head.appendChild(m)
  }
  if (theme === 'light') {
    make(SURFACE_LIGHT)
  } else if (theme === 'dark') {
    make(SURFACE_DARK)
  } else {
    make(SURFACE_LIGHT, '(prefers-color-scheme: light)')
    make(SURFACE_DARK, '(prefers-color-scheme: dark)')
  }
}
