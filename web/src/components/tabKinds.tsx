import type { ReactNode } from 'react'
import type { MenuItem } from './Dropdown'
import type { Tab, TabType } from '../store/tabs'

/** Per-tab live signal computed by the parent (App.tsx) from its stores.
 *  Pulled out as its own shape so [[TabKindHandler]] callbacks stay
 *  serializable / cheap to memoize. */
export interface TabBadges {
  /** Show the running spinner-dot (sessions with an in-flight agent turn). */
  running: boolean
  /** Show the unread accent dot. */
  unread: boolean
}

/**
 * Glue for one tab kind. The TabBar is purely presentational: it asks
 * the handler everything it needs to render and dispatch a tab. Adding
 * a new tab kind = adding a new entry to the registry assembled in
 * App.tsx. Per CLAUDE.md "Enforce Critical Invariants in the Type
 * System", `TabKindRegistry` is `Record<TabType, ...>`, so the compiler
 * refuses to build until every kind has glue wired up.
 */
export interface TabKindHandler {
  /** Whether `tab` is the currently active tab in the shell. Drives the
   *  `tab-active` class and suppresses the unread dot. */
  isActive: (tab: Tab) => boolean
  /** Live name from the relevant store, or `null` to fall through to
   *  the server-denormalized `tab.name`. */
  getLiveName: (tab: Tab) => string | null
  /** Running / unread flags. Receives `active` so handlers can avoid
   *  flashing the unread dot for the tab the user is currently on. */
  getBadges: (tab: Tab, active: boolean) => TabBadges
  /** Optional leading icon (project's split-rect glyph, report's
   *  document, …). Sessions return `null` to keep the chip compact. */
  getIcon: (tab: Tab) => ReactNode
  /** Click / Enter handler — navigate to the tab's target view. */
  /** Called when the tab's close affordance is used. Lets the kind
   *  clear its active id + navigate away when the closed tab is the
   *  currently-active one — otherwise the URL still points at the item
   *  and App.tsx's open-on-active effect immediately re-opens the tab.
   *  No-op when a background (non-active) tab is closed. */
  onClose: (tab: Tab) => void
  onActivate: (tab: Tab) => void
  /** Right-click + 3-dot menu items for this tab. The "Close tab" entry
   *  is layered in by the TabBar itself so every kind shares the same
   *  top item without having to remember to add it. */
  getMenuItems: (tab: Tab) => MenuItem[]
  /** Optional override for the close (×) button's tooltip. Return null
   *  to keep the default "Close tab". Temp sessions use this to warn
   *  that closing the tab deletes the session. */
  getCloseTitle?: (tab: Tab) => string | null
}

export type TabKindRegistry = Record<TabType, TabKindHandler>

// Stable shared icons — pulled out so they aren't recreated on every
// render. Same stroke style as the rail buttons in App.tsx for visual
// continuity.

const projectIcon: ReactNode = (
  <span className="tab-icon tab-icon-project" aria-hidden="true">
    ◧
  </span>
)

const reportIcon: ReactNode = (
  <span className="tab-icon tab-icon-report" aria-hidden="true">
    <svg
      width="14"
      height="14"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
      <polyline points="14 2 14 8 20 8" />
      <line x1="16" y1="13" x2="8" y2="13" />
      <line x1="16" y1="17" x2="8" y2="17" />
      <polyline points="10 9 9 9 8 9" />
    </svg>
  </span>
)

const repeatingTaskIcon: ReactNode = (
  <span className="tab-icon tab-icon-repeating-task" aria-hidden="true">
    <svg
      width="14"
      height="14"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <polyline points="1 4 1 10 7 10" />
      <polyline points="23 20 23 14 17 14" />
      <path d="M20.49 9A9 9 0 0 0 5.64 5.64L1 10m22 4l-4.64 4.36A9 9 0 0 1 3.51 15" />
    </svg>
  </span>
)

// Temp-session marker: an hourglass on the chip warns that closing this
// tab deletes the session (server-side, on last-tab-close).
const tempSessionIcon: ReactNode = (
  <span className="tab-icon tab-icon-temp-session" aria-hidden="true">
    <svg
      width="14"
      height="14"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <path d="M5 2h14" />
      <path d="M5 22h14" />
      <path d="M17 22v-4.172a2 2 0 0 0-.586-1.414L12 12l-4.414 4.414A2 2 0 0 0 7 17.828V22" />
      <path d="M7 2v4.172a2 2 0 0 0 .586 1.414L12 12l4.414-4.414A2 2 0 0 0 17 6.172V2" />
    </svg>
  </span>
)

export const tabIcons = {
  project: projectIcon,
  report: reportIcon,
  repeating_task: repeatingTaskIcon,
  tempSession: tempSessionIcon,
}

/** The default fallback label used when the live store has no name and
 *  the server-denormalized `tab.name` is empty (e.g. while an
 *  optimistic insert is in flight). */
export const tabDefaultLabel: Record<TabType, string> = {
  session: 'Session',
  project: 'Project',
  report: 'Report',
  repeating_task: 'Task',
}

/** Compose the encoded item_id for a report tab. The server splits this
 *  back into `(folder, file)` with the same `/` separator. */
export function reportTabId(folder: string, file: string): string {
  return `${folder}/${file}`
}

/** Reverse of [[reportTabId]]. Returns `null` if the id is malformed. */
export function parseReportTabId(itemId: string): { folder: string; file: string } | null {
  const idx = itemId.indexOf('/')
  if (idx <= 0 || idx === itemId.length - 1) return null
  return { folder: itemId.slice(0, idx), file: itemId.slice(idx + 1) }
}
