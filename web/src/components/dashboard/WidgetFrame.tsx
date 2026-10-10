import type { ReactNode } from 'react'
import { MenuButton, type MenuItem } from '../Dropdown'
import type { WidgetKind } from '../../store/views'
import type { WidgetContext } from './WidgetGrid'

const ICONS: Record<WidgetKind, ReactNode> = {
  session: (
    <path
      d="M2.5 3.5h11v7h-6l-3 2.5v-2.5h-2z"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.3"
    />
  ),
  terminal: (
    <>
      <rect
        x="1.5"
        y="2.5"
        width="13"
        height="11"
        rx="1.5"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.3"
      />
      <path d="M4.5 6l2 2-2 2M8 10.5h3.5" fill="none" stroke="currentColor" strokeWidth="1.3" />
    </>
  ),
  project: (
    <>
      <rect x="2" y="2.5" width="3.2" height="11" rx="0.8" fill="currentColor" opacity="0.9" />
      <rect x="6.4" y="2.5" width="3.2" height="7" rx="0.8" fill="currentColor" opacity="0.65" />
      <rect x="10.8" y="2.5" width="3.2" height="4.5" rx="0.8" fill="currentColor" opacity="0.4" />
    </>
  ),
}

interface Props {
  kind: WidgetKind
  /** Widget id, for tests and the grid's focus bookkeeping. */
  widgetId: string
  title: string
  /** Makes the title a link (e.g. a project widget opens its board). */
  onTitleClick?: () => void
  titleTestId?: string
  /** Live status chips beside the title (agent dot, terminal pill…). */
  statusSlot?: ReactNode
  menuItems: MenuItem[]
  ctx: WidgetContext
  /** Extra attributes on the root, e.g. `data-pane-id` / `data-terminal-id`. */
  dataAttrs?: Record<string, string | undefined>
  children: ReactNode
}

/**
 * Shared chrome for every dashboard widget: a compact header (kind icon,
 * title, status slot, ⋮ menu) that doubles as the drag handle, and a body.
 */
export default function WidgetFrame({
  kind,
  widgetId,
  title,
  onTitleClick,
  titleTestId,
  statusSlot,
  menuItems,
  ctx,
  dataAttrs,
  children,
}: Props) {
  return (
    <section
      className={`widget-frame${ctx.focused ? ' focused' : ''}${ctx.active ? ' widget-frame-active' : ''}`}
      data-testid="view-widget"
      data-kind={kind}
      data-widget-id={widgetId}
      data-focused={ctx.focused || undefined}
      aria-label={title}
      {...dataAttrs}
    >
      <header
        className={`widget-header${ctx.dragHandle ? ' widget-header-draggable' : ''}`}
        data-testid="widget-drag-handle"
        onPointerDown={(e) => {
          // Controls in the header keep their own clicks; the grip drags.
          const t = e.target as HTMLElement
          if (t.closest('button, a, input, select, [role="menuitem"]') && !t.closest('[data-grip]'))
            return
          ctx.dragHandle?.onPointerDown(e)
        }}
      >
        {ctx.grip ? (
          <button
            type="button"
            className="widget-grip"
            data-grip
            aria-label={`Move ${title} — arrow keys move, Shift+arrows resize`}
            title="Drag to move"
            onKeyDown={ctx.grip.onKeyDown}
          >
            <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
              {ICONS[kind]}
            </svg>
          </button>
        ) : (
          <span className="widget-grip" aria-hidden="true">
            <svg width="16" height="16" viewBox="0 0 16 16">
              {ICONS[kind]}
            </svg>
          </span>
        )}
        {onTitleClick ? (
          <button
            type="button"
            className="widget-title widget-title-link"
            title={title}
            data-testid={titleTestId}
            onClick={onTitleClick}
          >
            {title}
          </button>
        ) : (
          <h2 className="widget-title" title={title} data-testid={titleTestId}>
            {title}
          </h2>
        )}
        {statusSlot && <span className="widget-status">{statusSlot}</span>}
        <span className="widget-header-spacer" />
        <MenuButton
          items={menuItems}
          ariaLabel="Widget menu"
          triggerClassName="widget-menu-btn"
          testId="widget-menu"
        />
      </header>
      <div className="widget-body">{children}</div>
    </section>
  )
}
