import { useEffect, useRef, useState } from 'react'
import { useResourcesStore, type WorkflowInfo } from '../store/resources'

interface Props {
  /** Currently selected workflow id, or '' to mean "use the project default". */
  value: string
  onChange: (id: string) => void
  /** When set, shows a "Default workflow" option that resolves to this id —
   *  used in the card form so the card can inherit the project's default. */
  projectDefaultId?: string | null
  /** When set, shown as the label on the inherit option (e.g. the project
   *  default workflow's display name). */
  projectDefaultName?: string | null
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
  projectDefaultId,
  projectDefaultName,
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
  const defaultWf =
    projectDefaultId != null ? (sorted.find((w) => w.id === projectDefaultId) ?? null) : null
  const defaultLabel = projectDefaultName ?? defaultWf?.name ?? 'project default'

  const triggerLabel = selected
    ? selected.name
    : projectDefaultId != null
      ? `Default workflow (${defaultLabel})`
      : 'Default workflow'

  const triggerDescription = selected?.description ?? defaultWf?.description ?? ''

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
          {projectDefaultId != null && (
            <WorkflowOption
              active={value === ''}
              name={`Default workflow (${defaultLabel})`}
              description={
                defaultWf?.description ?? "Use whatever the project's default workflow is."
              }
              onClick={() => {
                onChange('')
                setOpen(false)
              }}
            />
          )}
          {projectDefaultId == null && (
            <WorkflowOption
              active={value === ''}
              name="Default workflow"
              description="Use the project's default workflow."
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
