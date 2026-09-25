import { useSyncExternalStore } from 'react'

/** True when motion should be suppressed: the `data-motion='reduce'` root
 *  attribute (Settings → Appearance, stamped by util/appearance.ts) or the
 *  OS `prefers-reduced-motion` preference. For JS-driven animation that the
 *  global reduced-motion.css rules can't reach (timers, deferred removal). */
export function prefersReducedMotion(): boolean {
  if (typeof window === 'undefined') return false
  if (document.documentElement.dataset.motion === 'reduce') return true
  return window.matchMedia?.('(prefers-reduced-motion: reduce)').matches ?? false
}

function subscribe(onChange: () => void): () => void {
  const mq = window.matchMedia?.('(prefers-reduced-motion: reduce)')
  mq?.addEventListener('change', onChange)
  const mo = new MutationObserver(onChange)
  mo.observe(document.documentElement, { attributes: true, attributeFilter: ['data-motion'] })
  return () => {
    mq?.removeEventListener('change', onChange)
    mo.disconnect()
  }
}

export function useReducedMotion(): boolean {
  return useSyncExternalStore(subscribe, prefersReducedMotion, () => false)
}
