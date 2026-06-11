import { useEffect, useRef, useState } from 'react'
import { useResourcesStore, type WorkflowInfo } from '../store/resources'
import Dropdown, { type MenuItem } from './Dropdown'

interface Props {
  /** Currently selected workflow id. In the card form, '' means "inherit
   *  from the project". In the project modals (no `projectWorkflowId` prop),
   *  '' means "nothing picked yet" and the caller should treat it as invalid. */
  value: string
  onChange: (id: string) => void
  /** When set, surfaces a "Project workflow (<name>)" option that resolves
   *  to this id — used in the card form so a card can inherit its project's
   *  workflow. Omit in the project modals so the project must pick an
   *  actual workflow rather than deferring to itself. */
  projectWorkflowId?: string
  /** Display name of the project's workflow, shown on the inherit option. */
  projectWorkflowName?: string | null
  disabled?: boolean
  /** id passed through to the trigger button for label association. */
  id?: string
}

/**
 * Workflow picker. The trigger looks like a form input (name on top,
 * description underneath) so it slots into modal forms; the menu uses the
 * shared `Dropdown` primitive so every popup in the app behaves the same
 * way (portal-rendered, viewport-clamped, Esc/outside-click to close,
 * matching item chrome). The two-line item shape comes from
 * `MenuItem.description` — see Dropdown.tsx.
 */
export default function WorkflowSelect({
  value,
  onChange,
  projectWorkflowId,
  projectWorkflowName,
  disabled,
  id,
}: Props) {
  const workflows = useResourcesStore((s) => s.workflows)
  const fetchWorkflows = useResourcesStore((s) => s.fetchWorkflows)
  const [anchor, setAnchor] = useState<{ x: number; y: number; width: number } | null>(null)
  const triggerRef = useRef<HTMLButtonElement>(null)

  useEffect(() => {
    fetchWorkflows()
  }, [fetchWorkflows])

  const sorted = [...workflows].sort((a, b) => {
    if (a.priority !== b.priority) return a.priority - b.priority
    return a.name.localeCompare(b.name)
  })

  const selected = sorted.find((w) => w.id === value)
  const projectWf = projectWorkflowId
    ? (sorted.find((w) => w.id === projectWorkflowId) ?? null)
    : null
  const projectLabel = projectWorkflowName ?? projectWf?.name ?? 'project workflow'

  const triggerLabel = selected
    ? selected.name
    : projectWorkflowId
      ? `Project workflow (${projectLabel})`
      : 'Select a workflow…'

  const triggerDescription = selected?.description ?? projectWf?.description ?? ''

  const open = () => {
    const el = triggerRef.current
    if (!el || disabled) return
    const r = el.getBoundingClientRect()
    setAnchor({ x: r.left, y: r.bottom + 4, width: r.width })
  }
  const close = () => setAnchor(null)

  const items: MenuItem[] = [
    ...(projectWorkflowId
      ? [
          {
            label: `Project workflow (${projectLabel})`,
            description: projectWf?.description ?? "Use the project's workflow.",
            active: value === '',
            onSelect: () => onChange(''),
          } satisfies MenuItem,
        ]
      : []),
    ...sorted.map(
      (wf: WorkflowInfo) =>
        ({
          label: wf.name,
          description: wf.description,
          active: value === wf.id,
          onSelect: () => onChange(wf.id),
        }) satisfies MenuItem,
    ),
  ]

  return (
    <div className="workflow-select">
      <button
        ref={triggerRef}
        type="button"
        id={id}
        className="workflow-select-trigger form-input"
        onClick={() => (anchor ? close() : open())}
        disabled={disabled}
      >
        <span className="workflow-select-name">{triggerLabel}</span>
        {triggerDescription && <span className="workflow-select-desc">{triggerDescription}</span>}
      </button>
      {anchor && (
        <Dropdown
          anchor={{ x: anchor.x, y: anchor.y }}
          items={items}
          onClose={close}
          align="left"
          className="workflow-select-dropdown"
        />
      )}
    </div>
  )
}
