// Persisted appearance settings (theme, accent hue, font size, density,
// motion). initAppearance() runs from main.tsx before the first render so
// saved values are visible immediately on page load (no flash of default
// appearance); SettingsPage shares the same helpers when the user changes
// a value. All of these are per-browser preferences and live in
// localStorage — AGENTS.md keeps that class of state out of the DB.

import { applyThemeColor, type Theme } from './themeColor'

export const THEME_KEY = 'peckboard_theme'
export const HUE_KEY = 'peckboard_hue'
export const FONT_SIZE_KEY = 'peckboard_font_size'
export const DENSITY_KEY = 'peckboard_density'
export const MOTION_KEY = 'peckboard_motion'

// Kept in sync with the `--primary-hue` default in index.css.
const DEFAULT_HUE = 220

/** Named accent presets rendered as swatches in Settings → Appearance.
 *  Each is just a hue on the existing `--primary-hue` pipeline, so picking
 *  a preset and dialing the custom slider persist through the same key. */
export const ACCENT_PRESETS: { name: string; hue: number }[] = [
  { name: 'Cardinal', hue: 0 },
  { name: 'Robin', hue: 25 },
  { name: 'Goldfinch', hue: 45 },
  { name: 'Parrot', hue: 145 },
  { name: 'Kingfisher', hue: 195 },
  { name: 'Bluebird', hue: DEFAULT_HUE },
  { name: 'Starling', hue: 270 },
  { name: 'Flamingo', hue: 330 },
]

export type FontSize = 'small' | 'default' | 'large' | 'xlarge'
/** Root font-size per option. `null` = leave the browser default alone
 *  (16px unless the user changed it), so "Default" also respects a
 *  browser-level accessibility override. The design system is rem-based
 *  (`--text-*`), so scaling the root scales all type. */
export const FONT_SIZES: { id: FontSize; label: string; px: number | null }[] = [
  { id: 'small', label: 'Small', px: 15 },
  { id: 'default', label: 'Default', px: null },
  { id: 'large', label: 'Large', px: 17 },
  { id: 'xlarge', label: 'Extra large', px: 18 },
]

export type Density = 'comfortable' | 'compact'
export type MotionPref = 'system' | 'reduced'

export function getStoredTheme(): Theme {
  const stored = localStorage.getItem(THEME_KEY)
  if (stored === 'light' || stored === 'dark' || stored === 'auto') return stored
  return 'auto'
}

export function applyTheme(theme: Theme) {
  const root = document.documentElement
  if (theme === 'auto') {
    root.removeAttribute('data-theme')
  } else {
    root.setAttribute('data-theme', theme)
  }
  applyThemeColor(theme)
}

export function getStoredHue(): number {
  const stored = localStorage.getItem(HUE_KEY)
  if (stored !== null) {
    const n = parseInt(stored, 10)
    if (!isNaN(n) && n >= 0 && n <= 360) return n
  }
  return DEFAULT_HUE
}

export function applyHue(hue: number) {
  document.documentElement.style.setProperty('--primary-hue', String(hue))
}

export function getStoredFontSize(): FontSize {
  const stored = localStorage.getItem(FONT_SIZE_KEY)
  if (FONT_SIZES.some((f) => f.id === stored)) return stored as FontSize
  return 'default'
}

export function applyFontSize(size: FontSize) {
  const px = FONT_SIZES.find((f) => f.id === size)?.px ?? null
  // Clearing the inline style hands control back to the browser default.
  document.documentElement.style.fontSize = px === null ? '' : `${px}px`
}

export function setFontSize(size: FontSize) {
  localStorage.setItem(FONT_SIZE_KEY, size)
  applyFontSize(size)
}

export function getStoredDensity(): Density {
  return localStorage.getItem(DENSITY_KEY) === 'compact' ? 'compact' : 'comfortable'
}

export function applyDensity(density: Density) {
  const root = document.documentElement
  if (density === 'compact') {
    root.setAttribute('data-density', 'compact')
  } else {
    root.removeAttribute('data-density')
  }
}

export function setDensity(density: Density) {
  localStorage.setItem(DENSITY_KEY, density)
  applyDensity(density)
}

export function getStoredMotion(): MotionPref {
  return localStorage.getItem(MOTION_KEY) === 'reduced' ? 'reduced' : 'system'
}

const REDUCE_MOTION_QUERY = '(prefers-reduced-motion: reduce)'

/** Resolve the stored preference against the OS one and stamp the result
 *  on the root. `styles/reduced-motion.css` keys every rule off
 *  `[data-motion='reduce']`, so this attribute — not the media query — is
 *  what actually collapses animation. */
export function applyMotion(pref: MotionPref) {
  const reduce = pref === 'reduced' || window.matchMedia(REDUCE_MOTION_QUERY).matches
  const root = document.documentElement
  if (reduce) {
    root.setAttribute('data-motion', 'reduce')
  } else {
    root.removeAttribute('data-motion')
  }
}

export function setMotion(pref: MotionPref) {
  localStorage.setItem(MOTION_KEY, pref)
  applyMotion(pref)
}

export function initAppearance() {
  applyTheme(getStoredTheme())
  applyHue(getStoredHue())
  applyFontSize(getStoredFontSize())
  applyDensity(getStoredDensity())
  applyMotion(getStoredMotion())
  // Keep "System" in sync when the OS preference flips while the app is
  // open (also covers Playwright's emulateMedia mid-test).
  window.matchMedia(REDUCE_MOTION_QUERY).addEventListener('change', () => {
    applyMotion(getStoredMotion())
  })
}
