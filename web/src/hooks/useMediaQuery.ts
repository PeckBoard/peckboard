import { useSyncExternalStore } from 'react'

/**
 * Reactive `matchMedia` hook. Returns whether the given media query
 * currently matches and re-renders the caller whenever the answer
 * flips (window resize, devtools viewport swap, etc.).
 *
 * Backed by `useSyncExternalStore` so the snapshot is read
 * synchronously on every render — no `useState`+`useEffect` cascade,
 * and the first paint already reflects the real viewport (no
 * orientation flash). SSR / Node has no `window`; the server snapshot
 * defaults to `false`. The kanban falls back to mobile layout when
 * `false`, so first paint never overflows the viewport horizontally
 * even in environments that get the desktop answer wrong on first
 * read.
 */
export function useMediaQuery(query: string): boolean {
  return useSyncExternalStore(
    (onChange) => {
      if (typeof window === 'undefined' || !window.matchMedia) return () => {}
      const mql = window.matchMedia(query)
      mql.addEventListener('change', onChange)
      return () => mql.removeEventListener('change', onChange)
    },
    () => {
      if (typeof window === 'undefined' || !window.matchMedia) return false
      return window.matchMedia(query).matches
    },
    () => false,
  )
}
