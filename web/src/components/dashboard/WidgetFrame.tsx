import type { ReactNode } from 'react'
import { MenuButton, type MenuItem } from '../Dropdown'
import type { WidgetKind } from '../../store/views'
import type { WidgetContext } from './WidgetGrid'

/** Outline stroke shared by the line icons. */
const S = {
  fill: 'none',
  stroke: 'currentColor',
  strokeWidth: 1.3,
  strokeLinecap: 'round',
  strokeLinejoin: 'round',
} as const

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
  note: (
    <>
      <path d="M3 2.5h7l3 3v8H3z" {...S} />
      <path d="M5.5 7h5M5.5 9.5h5M5.5 12h3" {...S} />
    </>
  ),
  report: (
    <>
      <path d="M3.5 2h9v12h-9z" {...S} />
      <path d="M6 11V9M8 11V6.5M10 11V8" {...S} />
    </>
  ),
  background: (
    <>
      <circle cx="8" cy="8" r="5.5" {...S} />
      <path d="M8 5v3l2 1.5" {...S} />
    </>
  ),
  repeating: (
    <>
      <path d="M12.5 6.5A4.75 4.75 0 0 0 3.6 6M3.5 9.5a4.75 4.75 0 0 0 8.9.5" {...S} />
      <path d="M3.2 3.2v2.9h2.9M12.8 12.8V9.9H9.9" {...S} />
    </>
  ),
  todos: (
    <>
      <path d="M2.5 4.5l1.2 1.2 2-2.2M2.5 10l1.2 1.2 2-2.2" {...S} />
      <path d="M8 4.5h5.5M8 10h5.5" {...S} />
    </>
  ),
  attention: (
    <>
      <path d="M8 2.2l6 10.8H2z" {...S} />
      <path d="M8 6.5v3" {...S} />
      <circle cx="8" cy="11.3" r="0.7" fill="currentColor" />
    </>
  ),
  review_queue: (
    <>
      <path d="M2.5 4h7M2.5 8h7M2.5 12h4" {...S} />
      <path d="M9.5 11.5l1.6 1.6 3-3.4" {...S} />
    </>
  ),
  review_quality: (
    <>
      <path d="M2.5 2.5v11h11" {...S} />
      <path d="M5 10.5l2.5-3 2 1.8 3.5-4.3" {...S} />
    </>
  ),
  workers: (
    <>
      <circle cx="5.5" cy="5.5" r="2" {...S} />
      <circle cx="11" cy="6.5" r="1.6" {...S} />
      <path
        d="M1.8 13c.4-2.3 1.9-3.5 3.7-3.5s3.3 1.2 3.7 3.5M9.6 10.2c1.9-.4 3.9.4 4.6 2.8"
        {...S}
      />
    </>
  ),
  worktrees: (
    <>
      <circle cx="4.5" cy="3.5" r="1.5" {...S} />
      <circle cx="4.5" cy="12.5" r="1.5" {...S} />
      <circle cx="11.5" cy="5.5" r="1.5" {...S} />
      <path d="M4.5 5v6M11.5 7c0 2.5-2.5 3-7 4" {...S} />
    </>
  ),
  dependencies: (
    <>
      <rect x="1.8" y="2.5" width="4.5" height="3.5" rx="0.8" {...S} />
      <rect x="9.7" y="2.5" width="4.5" height="3.5" rx="0.8" {...S} />
      <rect x="5.75" y="10" width="4.5" height="3.5" rx="0.8" {...S} />
      <path d="M4 6l3 4M12 6l-3 4" {...S} />
    </>
  ),
  ssh_activity: (
    <>
      <path d="M1.5 8h3l1.8-4.5 3.4 9 1.8-4.5h3" {...S} />
    </>
  ),
  ssh_hosts: (
    <>
      <rect x="2" y="2.5" width="12" height="4.5" rx="1" {...S} />
      <rect x="2" y="9" width="12" height="4.5" rx="1" {...S} />
      <circle cx="4.7" cy="4.75" r="0.7" fill="currentColor" />
      <circle cx="4.7" cy="11.25" r="0.7" fill="currentColor" />
    </>
  ),
  prs: (
    <>
      <circle cx="4.5" cy="3.5" r="1.5" {...S} />
      <circle cx="4.5" cy="12.5" r="1.5" {...S} />
      <circle cx="11.5" cy="12.5" r="1.5" {...S} />
      <path d="M4.5 5v6M11.5 11V6.5c0-1.1-.9-2-2-2H7.5M9 3l-1.5 1.5L9 6" {...S} />
    </>
  ),
  orchestrators: (
    <>
      <circle cx="8" cy="8" r="5.5" {...S} />
      <circle cx="8" cy="8" r="2.5" {...S} />
      <circle cx="8" cy="8" r="0.7" fill="currentColor" />
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
  /** Header controls right of the spacer, before the ⋮ menu (e.g. the
   *  filter toggle). */
  actions?: ReactNode
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
  actions,
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
        {actions}
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
