import type { MenuItem } from '../Dropdown'
import ChatView from '../ChatView'
import { SessionPaneStatus } from '../panes'
import WidgetFrame from './WidgetFrame'
import type { WidgetContext } from './WidgetGrid'

/** A session's compact chat inside a dashboard widget. */
export default function SessionWidget({
  widgetId,
  sessionId,
  title,
  menuItems,
  ctx,
}: {
  widgetId: string
  sessionId: string
  title: string
  menuItems: MenuItem[]
  ctx: WidgetContext
}) {
  return (
    <WidgetFrame
      kind="session"
      widgetId={widgetId}
      title={title}
      statusSlot={<SessionPaneStatus sessionId={sessionId} />}
      menuItems={menuItems}
      ctx={ctx}
      dataAttrs={{ 'data-pane-id': sessionId }}
    >
      <ChatView sessionId={sessionId} compact shortcutsEnabled={ctx.focused} />
    </WidgetFrame>
  )
}
