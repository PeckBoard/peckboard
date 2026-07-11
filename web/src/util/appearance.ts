// Persisted appearance settings (theme + accent hue). initAppearance()
// runs from main.tsx before the first render so a saved theme/hue is
// visible immediately on page load (no flash of default colors);
// SettingsPage shares the same helpers when the user changes a value.

import { applyThemeColor, type Theme } from './themeColor'

export const THEME_KEY = 'peckboard_theme'
export const HUE_KEY = 'peckboard_hue'

// Kept in sync with the `--primary-hue` default in index.css.
const DEFAULT_HUE = 220

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

export function initAppearance() {
  applyTheme(getStoredTheme())
  applyHue(getStoredHue())
}
