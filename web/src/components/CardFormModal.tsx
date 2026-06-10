import { useEffect, useState, type FormEvent } from 'react'
import { useProjectsStore } from '../store/projects'
import { useResourcesStore } from '../store/resources'
import { authedFetch } from '../store/auth'
import type { Card } from '../types/api'
import WorkflowSelect from './WorkflowSelect'

interface CardFormBaseProps {
  projectId: string
  onClose: () => void
}

type CardFormProps =
  | (CardFormBaseProps & { mode: 'create'; card?: undefined })
  | (CardFormBaseProps & { mode: 'edit'; card: Card })

interface ModelInfo {
  id: string
  display_name: string
}

const EFFORT_OPTIONS = [
  { value: '', label: 'Default' },
  { value: 'low', label: 'Low' },
  { value: 'medium', label: 'Medium' },
  { value: 'high', label: 'High' },
  { value: 'xhigh', label: 'Extra high' },
  { value: 'max', label: 'Max' },
]

/**
 * One modal for both creating and editing a card. The two flows share so much
 * (workflow + model + effort + priority + dependencies + blocked state) that
 * keeping them in separate components meant edits to either could drift; this
 * is the single place those rules live.
 */
export default function CardFormModal(props: CardFormProps) {
  const { projectId, onClose, mode } = props
  const card = mode === 'edit' ? props.card : null

  const createCard = useProjectsStore((s) => s.createCard)
  const updateCard = useProjectsStore((s) => s.updateCard)
  const cards = useProjectsStore((s) => s.cards)
  const project = useProjectsStore((s) => s.projects.find((p) => p.id === projectId))

  const isTerminal = card?.step === 'done' || card?.step === 'wont_do'
  const isBacklog = mode === 'create' || card?.step === 'backlog'

  const [title, setTitle] = useState(card?.title ?? '')
  const [description, setDescription] = useState(card?.description ?? '')
  const [priority, setPriority] = useState(card?.priority ?? 2)
  const [workflow, setWorkflow] = useState(card?.workflow ?? '')
  const [model, setModel] = useState(card?.model ?? '')
  const [effort, setEffort] = useState(card?.effort ?? '')
  const [blocked, setBlocked] = useState(card?.blocked ?? false)
  const [blockReason, setBlockReason] = useState(card?.block_reason ?? '')
  const [dependsOn, setDependsOn] = useState<string[]>(card?.depends_on ?? [])
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)

  const dependencyCandidates = cards.filter((c) => c.id !== card?.id)

  const workflows = useResourcesStore((s) => s.workflows)
  const fetchWorkflows = useResourcesStore((s) => s.fetchWorkflows)
  const models = useResourcesStore((s) => s.models)
  const fetchModels = useResourcesStore((s) => s.fetchModels)
  const [priorities, setPriorities] = useState<{ label: string; value: number }[]>([
    { label: 'Critical', value: 0 },
    { label: 'High', value: 1 },
    { label: 'Medium', value: 2 },
    { label: 'Low', value: 3 },
  ])

  useEffect(() => {
    fetchWorkflows()
    fetchModels()
    authedFetch('/api/priorities')
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (data?.priorities) setPriorities(data.priorities)
      })
      .catch(() => {})
  }, [fetchWorkflows, fetchModels])

  const projectDefaultName = workflows.find((w) => w.id === project?.default_workflow)?.name ?? null

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    if (!title.trim()) {
      setError('Title is required')
      return
    }
    setLoading(true)
    setError('')
    try {
      if (mode === 'create') {
        await createCard(projectId, {
          title: title.trim(),
          description: description.trim(),
          step: 'backlog',
          priority,
          workflow: workflow || undefined,
          model: model || undefined,
          effort: effort || undefined,
          depends_on: dependsOn.length > 0 ? dependsOn : undefined,
        } as Partial<Card>)
      } else {
        const updates: Partial<Card> = {
          title: title.trim(),
          priority,
          blocked,
          block_reason: blocked ? blockReason.trim() || null : null,
          model: model || null,
          effort: effort || null,
          depends_on: dependsOn,
        }
        if (isBacklog) {
          updates.description = description.trim()
          updates.workflow = workflow || null
        }
        await updateCard(projectId, card!.id, updates)
      }
      onClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to save card')
    } finally {
      setLoading(false)
    }
  }

  if (mode === 'edit' && isTerminal) {
    return (
      <div className="modal-backdrop" onClick={onClose}>
        <div className="modal" onClick={(e) => e.stopPropagation()} style={{ maxWidth: 440 }}>
          <h2>Card Details</h2>
          <p className="form-hint" style={{ marginBottom: 12 }}>
            Cards in terminal state (done / won't do) are read-only.
          </p>
          <div className="card-detail-grid">
            <div className="card-detail-row">
              <span className="card-detail-label">Title</span>
              <span>{card!.title}</span>
            </div>
            <div className="card-detail-row">
              <span className="card-detail-label">Step</span>
              <span>{card!.step}</span>
            </div>
            {card!.description && (
              <div className="card-detail-row">
                <span className="card-detail-label">Description</span>
                <span>{card!.description}</span>
              </div>
            )}
            {card!.workflow && (
              <div className="card-detail-row">
                <span className="card-detail-label">Workflow</span>
                <span>{card!.workflow}</span>
              </div>
            )}
          </div>
          <div className="form-actions">
            <button type="button" className="btn-secondary" onClick={onClose}>
              Close
            </button>
          </div>
        </div>
      </div>
    )
  }

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()} style={{ maxWidth: 520 }}>
        <h2>{mode === 'create' ? 'New Card' : 'Edit Card'}</h2>
        <form onSubmit={handleSubmit}>
          <div className="form-field">
            <label className="form-label">Title</label>
            <input
              className="form-input"
              value={title}
              onChange={(e) => setTitle(e.target.value)}
              autoFocus={mode === 'create'}
              required
            />
          </div>
          <div className="form-field">
            <label className="form-label">
              Description {!isBacklog && <span className="optional">(locked)</span>}
            </label>
            <textarea
              className="form-input card-form-description"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              rows={8}
              disabled={!isBacklog}
            />
          </div>
          <div className="form-field">
            <label className="form-label">Priority</label>
            <select
              className="form-input"
              value={priority}
              onChange={(e) => setPriority(Number(e.target.value))}
            >
              {priorities.map((p) => (
                <option key={p.value} value={p.value}>
                  {p.label}
                </option>
              ))}
            </select>
          </div>
          <div className="form-field">
            <label className="form-label">
              Workflow {!isBacklog && <span className="optional">(locked)</span>}
            </label>
            <WorkflowSelect
              value={workflow}
              onChange={setWorkflow}
              projectDefaultId={project?.default_workflow ?? null}
              projectDefaultName={projectDefaultName}
              disabled={!isBacklog}
            />
          </div>
          <div className="form-field">
            <label className="form-label">Model</label>
            <select className="form-input" value={model} onChange={(e) => setModel(e.target.value)}>
              <option value="">Default</option>
              {(models as ModelInfo[]).map((m) => (
                <option key={m.id} value={m.id}>
                  {m.display_name}
                </option>
              ))}
            </select>
          </div>
          <div className="form-field">
            <label className="form-label">Effort</label>
            <select
              className="form-input"
              value={effort}
              onChange={(e) => setEffort(e.target.value)}
            >
              {EFFORT_OPTIONS.map((o) => (
                <option key={o.value} value={o.value}>
                  {o.label}
                </option>
              ))}
            </select>
          </div>
          {mode === 'edit' && (
            <div className="form-field">
              <label className="form-checkbox-label">
                <input
                  type="checkbox"
                  checked={blocked}
                  onChange={(e) => setBlocked(e.target.checked)}
                />
                <span>Blocked</span>
              </label>
              {blocked && (
                <input
                  className="form-input"
                  style={{ marginTop: 6 }}
                  placeholder="Block reason..."
                  value={blockReason}
                  onChange={(e) => setBlockReason(e.target.value)}
                />
              )}
            </div>
          )}
          {dependencyCandidates.length > 0 && (
            <div className="form-field">
              <label className="form-label">Depends On</label>
              <p className="form-hint" style={{ marginTop: 0, marginBottom: 6 }}>
                A worker only starts this card once every selected card is done.
              </p>
              <div className="kanban-deps-options">
                {dependencyCandidates.map((c) => (
                  <label key={c.id} className="kanban-dep-option">
                    <input
                      type="checkbox"
                      checked={dependsOn.includes(c.id)}
                      onChange={(e) =>
                        setDependsOn((prev) =>
                          e.target.checked ? [...prev, c.id] : prev.filter((id) => id !== c.id),
                        )
                      }
                    />
                    <span>
                      {c.title}
                      {c.step === 'done' ? ' (done)' : ''}
                    </span>
                  </label>
                ))}
              </div>
            </div>
          )}
          {error && <p className="form-error">{error}</p>}
          <div className="form-actions">
            <button type="button" className="btn-secondary" onClick={onClose}>
              Cancel
            </button>
            <button type="submit" className="btn-primary" disabled={loading || !title.trim()}>
              {loading
                ? mode === 'create'
                  ? 'Creating...'
                  : 'Saving...'
                : mode === 'create'
                  ? 'Create Card'
                  : 'Save'}
            </button>
          </div>
        </form>
      </div>
    </div>
  )
}
