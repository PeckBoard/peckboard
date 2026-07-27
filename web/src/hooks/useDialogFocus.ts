import { useEffect, useRef, type RefObject } from 'react'

/**
 * Focus management for modal dialogs — initial focus, a Tab trap, and
 * focus restore on close. Used by the two dialog primitives (`Modal`,
 * `ConfirmDialog`) so every dialog in the app inherits the behaviour;
 * call sites should never need their own copy.
 */

const FOCUSABLE = [
  'a[href]',
  'area[href]',
  'button:not([disabled])',
  'input:not([disabled]):not([type="hidden"])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  'iframe',
  'audio[controls]',
  'video[controls]',
  '[contenteditable]:not([contenteditable="false"])',
  '[tabindex]:not([tabindex="-1"])',
].join(', ')

/** Popup surfaces that portal to `<body>` while logically living inside a
 *  dialog (the model picker, a row's context menu). Focus legitimately
 *  leaves the panel for these, so the trap must not yank it back. */
const PORTALLED_POPUPS = '.dropdown-menu, [role="menu"], [role="listbox"]'

/** Open dialog panels, outermost first. Only the top-most one traps Tab,
 *  so a ConfirmDialog stacked on a Modal cycles within itself. */
const openPanels: HTMLElement[] = []

/**
 * The last few elements to hold focus, most recent first.
 *
 * Needed because the element we want to return focus to is usually gone
 * by the time the dialog mounts: clicking a 3-dot menu item unmounts the
 * `Dropdown` in the same commit that mounts the dialog, and React runs
 * unmount cleanup before mount effects — so `document.activeElement` is
 * already `<body>`. Walking back through the history finds the trigger
 * that opened the menu, which is still on the page.
 */
const focusHistory: HTMLElement[] = []
const HISTORY_LIMIT = 5

if (typeof document !== 'undefined') {
  document.addEventListener(
    'focusin',
    (e) => {
      const el = e.target as HTMLElement | null
      if (!el || el === document.body || typeof el.focus !== 'function') return
      const at = focusHistory.indexOf(el)
      if (at >= 0) focusHistory.splice(at, 1)
      focusHistory.unshift(el)
      if (focusHistory.length > HISTORY_LIMIT) focusHistory.length = HISTORY_LIMIT
    },
    true,
  )
}

function isVisible(el: HTMLElement): boolean {
  return el.offsetWidth > 0 || el.offsetHeight > 0 || el.getClientRects().length > 0
}

function focusableWithin(panel: HTMLElement): HTMLElement[] {
  return Array.from(panel.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
    (el) => el.getAttribute('aria-hidden') !== 'true' && isVisible(el),
  )
}

/**
 * Returns a ref to attach to the dialog panel. While mounted the panel
 * holds focus: it takes focus on open (unless a child already claimed it
 * via `autoFocus`), Tab and Shift+Tab cycle within it, and on close focus
 * goes back to whatever opened the dialog.
 */
export default function useDialogFocus<T extends HTMLElement>(): RefObject<T | null> {
  const ref = useRef<T | null>(null)

  useEffect(() => {
    const panel = ref.current
    if (!panel) return

    const restoreCandidates: HTMLElement[] = []
    const previous = document.activeElement
    if (previous instanceof HTMLElement && previous !== document.body) {
      restoreCandidates.push(previous)
    }
    restoreCandidates.push(...focusHistory)

    openPanels.push(panel)

    // Don't steal focus from a child that already asked for it (React's
    // `autoFocus`), which is how a danger ConfirmDialog lands on Cancel.
    if (!panel.contains(document.activeElement)) {
      const first = focusableWithin(panel)[0]
      if (first) {
        first.focus()
      } else {
        panel.tabIndex = -1
        panel.focus()
      }
    }

    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key !== 'Tab' || e.defaultPrevented) return
      if (openPanels[openPanels.length - 1] !== panel) return
      const active = document.activeElement as HTMLElement | null
      if (active?.closest(PORTALLED_POPUPS)) return

      const items = focusableWithin(panel)
      if (items.length === 0) {
        e.preventDefault()
        panel.tabIndex = -1
        panel.focus()
        return
      }
      const first = items[0]
      const last = items[items.length - 1]

      if (!active || !panel.contains(active)) {
        e.preventDefault()
        ;(e.shiftKey ? last : first).focus()
      } else if (e.shiftKey && (active === first || active === panel)) {
        e.preventDefault()
        last.focus()
      } else if (!e.shiftKey && active === last) {
        e.preventDefault()
        first.focus()
      }
    }

    document.addEventListener('keydown', onKeyDown, true)

    return () => {
      document.removeEventListener('keydown', onKeyDown, true)
      const at = openPanels.indexOf(panel)
      if (at >= 0) openPanels.splice(at, 1)

      // Only take focus back if the dialog still had it — if something
      // else has already claimed focus, leave it alone.
      const active = document.activeElement
      if (active && active !== document.body && !panel.contains(active)) return

      const target = restoreCandidates.find(
        (el) => el.isConnected && !panel.contains(el) && isVisible(el),
      )
      target?.focus()
    }
  }, [])

  return ref
}
