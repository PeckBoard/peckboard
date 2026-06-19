import type { ReactNode } from 'react'

interface ListViewHeaderProps {
  title: ReactNode
  /** Label for the primary action button (e.g. "+ New session"). */
  actionLabel?: ReactNode
  /** Handler for the primary action; required to render the button. */
  onAction?: () => void
  /** Extra control nodes rendered to the right of the title — e.g. a
   *  filter toggle or a secondary button. Render to the LEFT of the
   *  primary action button. */
  extras?: ReactNode
}

/**
 * Standard heading bar for a list view (Sessions, Projects, Experts,
 * Repeating Tasks, etc). Renders the conventional
 * `list-view-header` / `list-view-title` / `list-view-action`
 * markup so every top-level list page reads with the same shape.
 *
 * Keep this lean: any view that needs extra structure (status badges,
 * custom widgets) can either pass `extras` or fall back to writing
 * the markup directly.
 */
export default function ListViewHeader({
  title,
  actionLabel,
  onAction,
  extras,
}: ListViewHeaderProps) {
  return (
    <div className="list-view-header">
      <h2 className="list-view-title">{title}</h2>
      {extras}
      {actionLabel && onAction && (
        <button className="list-view-action" onClick={onAction}>
          {actionLabel}
        </button>
      )}
    </div>
  )
}
