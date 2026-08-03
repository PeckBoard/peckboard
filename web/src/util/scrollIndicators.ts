// Global scroll-edge indicators. Scrollbars are hidden site-wide
// (index.css), so the "there's more content this way" cue is the edge
// fade this module drives: every overflowing scroll container gets
// data-scroll="up down left right" listing only the sides with more
// content, and index.css masks those edges with a fade.
//
// No per-component wiring: a capture-phase scroll listener updates the
// scrolled element immediately (rAF-coalesced), and a throttled
// full-document sweep — triggered by mutations, resizes, transition/
// animation ends and media loads — picks up containers that become
// scrollable without ever having been scrolled. Containers can opt out
// of the fade (scrollbars stay hidden) with
// data-scroll-indicators="off".

const ATTR = 'data-scroll'

// 1px slack: fractional layout sizes leave scrollHeight/scrollTop
// disagreeing by <1px at the scroll extremes.
const EPS = 1

/** Elements currently stamped with the attribute, for stale cleanup. */
const tracked = new Set<Element>()
/** Scrolled elements awaiting their next-frame re-check. */
const dirty = new Set<Element>()

function eligible(el: Element): el is HTMLElement {
  if (!(el instanceof HTMLElement)) return false
  // The page itself never scrolls (full-viewport flex layout); masking
  // <html> would also fade fixed overlays.
  if (el === document.documentElement || el === document.body) return false
  if (el.getAttribute('data-scroll-indicators') === 'off') return false
  // Masking native form controls is glitchy across engines, and the
  // composer textarea already manages its own overflow (InputBar).
  if (
    el instanceof HTMLTextAreaElement ||
    el instanceof HTMLInputElement ||
    el instanceof HTMLSelectElement
  )
    return false
  return true
}

/** The sides of `el` that have more content, as the attribute value. */
function sides(el: HTMLElement): string {
  const style = getComputedStyle(el)
  const parts: string[] = []
  if (
    el.scrollHeight - el.clientHeight > EPS &&
    (style.overflowY === 'auto' || style.overflowY === 'scroll')
  ) {
    if (el.scrollTop > EPS) parts.push('up')
    if (el.scrollTop + el.clientHeight < el.scrollHeight - EPS) parts.push('down')
  }
  if (
    el.scrollWidth - el.clientWidth > EPS &&
    (style.overflowX === 'auto' || style.overflowX === 'scroll')
  ) {
    if (el.scrollLeft > EPS) parts.push('left')
    if (el.scrollLeft + el.clientWidth < el.scrollWidth - EPS) parts.push('right')
  }
  return parts.join(' ')
}

function untrack(el: Element) {
  if (el.hasAttribute(ATTR)) el.removeAttribute(ATTR)
  tracked.delete(el)
}

function update(el: Element) {
  if (!el.isConnected || !eligible(el)) {
    untrack(el)
    return
  }
  // Cheap size check first — getComputedStyle only for real overflowers.
  const overflowing =
    el.scrollHeight - el.clientHeight > EPS || el.scrollWidth - el.clientWidth > EPS
  const next = overflowing ? sides(el) : ''
  if (next) {
    if (el.getAttribute(ATTR) !== next) el.setAttribute(ATTR, next)
    tracked.add(el)
  } else {
    untrack(el)
  }
}

let rafId: number | undefined
function scheduleFlush() {
  if (rafId !== undefined) return
  rafId = requestAnimationFrame(() => {
    rafId = undefined
    const els = [...dirty]
    dirty.clear()
    els.forEach(update)
  })
}

let sweepTimer: number | undefined
function scheduleSweep() {
  // Fixed trailing delay (throttle, not resetting debounce) so a
  // constant mutation stream — e.g. a streaming chat reply — still
  // gets swept regularly instead of starving the timer forever.
  if (sweepTimer !== undefined) return
  sweepTimer = window.setTimeout(() => {
    sweepTimer = undefined
    sweep()
  }, 150)
}

function sweep() {
  const stale = new Set(tracked)
  for (const el of document.body.querySelectorAll('*')) {
    if (el.scrollHeight - el.clientHeight > EPS || el.scrollWidth - el.clientWidth > EPS) {
      update(el)
      stale.delete(el)
    }
  }
  // Whatever the sweep didn't visit no longer overflows (or left the
  // DOM) — clear its fades.
  for (const el of stale) untrack(el)
}

export function initScrollIndicators() {
  // Scroll events don't bubble, but they are observable in the capture
  // phase — one listener covers every scroller in the app.
  document.addEventListener(
    'scroll',
    (e) => {
      if (e.target instanceof Element) {
        dirty.add(e.target)
        scheduleFlush()
      }
    },
    { capture: true, passive: true },
  )
  // Size changes that arrive without a scroll: CSS transitions/
  // animations settling and images/media finishing their load.
  for (const type of ['transitionend', 'animationend', 'load']) {
    document.addEventListener(type, scheduleSweep, {
      capture: true,
      passive: true,
    })
  }
  window.addEventListener('resize', scheduleSweep)

  const observer = new MutationObserver((mutations) => {
    for (const m of mutations) {
      // Our own data-scroll writes come back through the observer —
      // ignore them or every sweep would schedule the next one.
      if (m.type === 'attributes' && m.attributeName === ATTR) continue
      scheduleSweep()
      return
    }
  })
  observer.observe(document.body, {
    subtree: true,
    childList: true,
    attributes: true,
    characterData: true,
  })

  sweep()
}
