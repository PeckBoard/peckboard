/**
 * The single keyboard model shared by every `role="menu"` popup in the app —
 * `Dropdown` (3-dot / MenuButton), `ContextMenuView` (right-click) and
 * `PriorityChevron`. ARIA promises arrow navigation the moment a component
 * claims menu semantics; hand-rolling that per popup is how three different
 * half-implementations happen.
 *
 * Contract for a menu popup:
 *  - on open, focus the active item (or the first enabled one),
 *  - ArrowUp/ArrowDown wrap, Home/End jump,
 *  - Enter/Space activate (native `<button>` behaviour — nothing to add),
 *  - Escape closes and the OWNER returns focus to the trigger,
 *  - the trigger carries `aria-haspopup="menu"` + `aria-expanded`.
 *
 * Listbox popups (`ModelPicker`) keep their own input-driven highlight model:
 * there focus stays in the filter input and `aria-activedescendant` moves.
 */

/** Enabled, focusable rows of a menu popup. */
export const MENU_ITEM_SELECTOR =
  '[role="menuitem"]:not(:disabled),[role="menuitemradio"]:not(:disabled),[role="menuitemcheckbox"]:not(:disabled)'

/** Marks the row a menu should land on when it opens (current value). */
export const MENU_ACTIVE_ATTR = 'data-menu-active'

export function menuItemElements(root: HTMLElement | null): HTMLElement[] {
  if (!root) return []
  return Array.from(root.querySelectorAll<HTMLElement>(MENU_ITEM_SELECTOR))
}

/**
 * Land focus inside a freshly-opened menu: on the active row if one is
 * marked, else the first enabled row. Menus portal to the end of `<body>`,
 * so without this a keyboard user would have to Tab through the whole app
 * to reach the popup they just opened.
 */
export function focusFirstMenuItem(root: HTMLElement | null): void {
  const els = menuItemElements(root)
  if (els.length === 0) return
  const active = els.find((el) => el.getAttribute(MENU_ACTIVE_ATTR) === 'true')
  ;(active ?? els[0]).focus()
}

/**
 * Roving-focus key handling for a menu popup. Returns true when the key was
 * consumed so callers can add their own extras (ArrowRight to open a
 * submenu, Escape to close) without re-checking the common cases.
 *
 * No-ops unless focus is actually inside `root`: a submenu is portalled to
 * `<body>` but is still a React child of its parent menu, so its key events
 * bubble into the parent's handler and would otherwise yank focus back out.
 */
export function handleMenuKeys(
  e: { key: string; preventDefault: () => void },
  root: HTMLElement | null,
): boolean {
  if (!root || !root.contains(document.activeElement)) return false
  const els = menuItemElements(root)
  if (els.length === 0) return false
  const at = els.indexOf(document.activeElement as HTMLElement)
  switch (e.key) {
    case 'ArrowDown':
      e.preventDefault()
      els[at < 0 ? 0 : (at + 1) % els.length].focus()
      return true
    case 'ArrowUp':
      e.preventDefault()
      els[at <= 0 ? els.length - 1 : at - 1].focus()
      return true
    case 'Home':
      e.preventDefault()
      els[0].focus()
      return true
    case 'End':
      e.preventDefault()
      els[els.length - 1].focus()
      return true
    default:
      return false
  }
}
