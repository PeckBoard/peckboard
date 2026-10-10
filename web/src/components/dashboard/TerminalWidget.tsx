import { Suspense, lazy, type ReactNode } from 'react'
import type { MenuItem } from '../Dropdown'
import type { TerminalPhase, TerminalStatus } from '../../store/terminals'
import type { ViewTerminalMeta } from '../../store/views'
import { TerminalStatusPill } from '../terminal/TerminalBadges'
import WidgetFrame from './WidgetFrame'
import type { WidgetContext } from './WidgetGrid'
// xterm is heavy: only load it once a view actually shows a terminal.
const ViewTerminalPane = lazy(() => import('../terminal/ViewTerminalPane'))

/** Header status for a terminal widget: host label + live / reconnecting /
 *  ended pill. */
function TerminalPaneStatus({
  meta,
  phase,
}: {
  meta: ViewTerminalMeta | undefined
  phase: TerminalPhase
}) {
  return (
    <>
      {meta && <span className="view-terminal-host">{meta.host_label}</span>}
      <TerminalStatusPill phase={meta?.closed ? 'ended' : phase} />
    </>
  )
}

/** A live shell inside a dashboard widget; closed terminals offer a reopen. */
export default function TerminalWidget({
  widgetId,
  terminalId,
  meta,
  phase,
  menuItems,
  ctx,
  onStatus,
  onReopen,
  replaceSlot,
}: {
  widgetId: string
  terminalId: string
  meta: ViewTerminalMeta | undefined
  phase: TerminalPhase
  menuItems: MenuItem[]
  ctx: WidgetContext
  onStatus: (s: TerminalStatus) => void
  onReopen: () => Promise<void>
  replaceSlot: ReactNode
}) {
  return (
    <WidgetFrame
      kind="terminal"
      widgetId={widgetId}
      title={meta?.name ?? 'Terminal'}
      statusSlot={<TerminalPaneStatus meta={meta} phase={phase} />}
      menuItems={menuItems}
      ctx={ctx}
    >
      <Suspense fallback={null}>
        <ViewTerminalPane
          terminalId={terminalId}
          meta={meta}
          focused={ctx.focused}
          visible
          onStatus={onStatus}
          onReopen={onReopen}
          replaceSlot={replaceSlot}
        />
      </Suspense>
    </WidgetFrame>
  )
}
