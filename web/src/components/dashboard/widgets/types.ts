import type { MenuItem } from '../../Dropdown'
import type { ViewWidget, WidgetPatch } from '../../../store/views'
import type { WidgetContext } from '../WidgetGrid'

/** Props every info widget (note, report, attention, workers, …) takes.
 *  Each widget renders its own `WidgetFrame` with `menuItems` (the page's
 *  standard Configure… / Remove items) and fetches its own data. */
export interface InfoWidgetProps {
  widget: ViewWidget
  ctx: WidgetContext
  menuItems: MenuItem[]
  /** Project scope for scoped kinds; null = all projects. */
  scopeProjectId: string | null
  /** Display name of the scope project, when scoped. */
  scopeName?: string
  onOpenSession: (sessionId: string) => void
  onOpenProject: (projectId: string) => void
  /** Persist widget-owned fields (e.g. a note's `body`, filter-bar state). */
  onChange: (patch: WidgetPatch) => void
  /** Open a terminal tab (ssh widgets), when the page provides it. */
  onOpenTerminal?: (pluginId: string, hostId: string) => void
  /** Open a report in its own tab (report widget), when the page provides it. */
  onOpenReport?: (folder: string, file: string) => void
}
