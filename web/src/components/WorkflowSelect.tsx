import { useEffect, useRef, useState } from 'react'
import { useResourcesStore, type WorkflowInfo } from '../store/resources'

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
 * Workflow picker that surfaces each workflow's description below its name,
 * so the user can tell Task from Research from Breakdown without having to
 * pick blindly. Mirrors the chat-toolbar dropdown look: a borderless trigger
 * that pops a fixed-position menu showing one card per workflow.
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
  const [open, setOpen] = useState(false)
  const wrapperRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    fetchWorkflows()
  }, [fetchWorkflows])

  useEffect(() => {
    if (!open) return
    const onClick = (e: MouseEvent) => {
      if (!wrapperRef.current?.contains(e.target as Node)) setOpen(false)
    }
    document.addEventListener('mousedown', onClick)
    return () => document.removeEventListener('mousedown', onClick)
  }, [open])

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

  return (
    <div className="workflow-select" ref={wrapperRef}>
      <button
        type="button"
        id={id}
        className="workflow-select-trigger form-input"
        onClick={() => !disabled && setOpen(!open)}
        disabled={disabled}
      >
        <span className="workflow-select-name">{triggerLabel}</span>
        {triggerDescription && <span className="workflow-select-desc">{triggerDescription}</span>}
      </button>
      {open && (
        <div className="workflow-select-menu">
          {projectWorkflowId && (
            <WorkflowOption
              active={value === ''}
              name={`Project workflow (${projectLabel})`}
              description={projectWf?.description ?? "Use the project's workflow."}
              onClick={() => {
                onChange('')
                setOpen(false)
              }}
            />
          )}
          {sorted.map((wf: WorkflowInfo) => (
            <WorkflowOption
              key={wf.id}
              active={value === wf.id}
              name={wf.name}
              description={wf.description}
              onClick={() => {
                onChange(wf.id)
                setOpen(false)
              }}
            />
          ))}
        </div>
      )}
    </div>
  )
}

function WorkflowOption({
  active,
  name,
  description,
  onClick,
}: {
  active: boolean
  name: string
  description: string
  onClick: () => void
}) {
  return (
    <button
      type="button"
      className={`workflow-select-option${active ? ' active' : ''}`}
      onClick={onClick}
    >
      <span className="workflow-select-option-name">{name}</span>
      {description && <span className="workflow-select-option-desc">{description}</span>}
    </button>
  )
}
