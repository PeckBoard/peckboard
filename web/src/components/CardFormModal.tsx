import { useEffect, useMemo, useState, type FormEvent } from 'react'
import { useProjectsStore } from '../store/projects'
import { effortOptionsForModel, useResourcesStore } from '../store/resources'
import { authedFetch } from '../store/auth'
import type { Card } from '../types/api'
import DependencyPickerModal from './DependencyPickerModal'
import Modal from './Modal'
import ModelPicker from './ModelPicker'
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
  const [pickerOpen, setPickerOpen] = useState(false)

  const dependencyCandidates = cards.filter((c) => c.id !== card?.id)
  const selectedDependencies = useMemo(() => {
    const byId = new Map(dependencyCandidates.map((c) => [c.id, c]))
    return dependsOn.map((id) => byId.get(id)).filter((c): c is Card => c != null)
  }, [dependencyCandidates, dependsOn])

  const workflows = useResourcesStore((s) => s.workflows)
  const fetchWorkflows = useResourcesStore((s) => s.fetchWorkflows)
  const models = useResourcesStore((s) => s.models)
  const providers = useResourcesStore((s) => s.providers)
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

  // Effort options follow the chosen model's provider.
  const effortOptions = useMemo(() => effortOptionsForModel(model, providers), [model, providers])
  // Clear a now-invalid effort back to Default on model change so we never
  // save one the provider can't use.
  const handleModelChange = (id: string) => {
    setModel(id)
    const opts = effortOptionsForModel(id, providers)
    if (providers.length > 0 && effort && !opts.some((o) => o.value === effort)) setEffort('')
  }

  const projectWorkflowId = project?.workflow
  const projectWorkflowName = workflows.find((w) => w.id === projectWorkflowId)?.name ?? null

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
          blocked,
          block_reason: blocked ? blockReason.trim() || null : null,
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
          // card.workflow is NOT NULL — when the picker is set to the
          // inherit option (empty string), resolve to the project's
          // workflow id rather than sending an empty value the backend
          // would reject.
          updates.workflow = workflow || project?.workflow
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
      <Modal onClose={onClose} maxWidth={440}>
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
      </Modal>
    )
  }

  return (
    <>
      <Modal onClose={onClose} maxWidth={520}>
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
              projectWorkflowId={projectWorkflowId ?? undefined}
              projectWorkflowName={projectWorkflowName}
              disabled={!isBacklog}
            />
          </div>
          <div className="form-field">
            <label className="form-label">Model</label>
            <ModelPicker
              value={model}
              onChange={handleModelChange}
              models={models as ModelInfo[]}
              testId="card-model"
            />
          </div>
          <div className="form-field">
            <label className="form-label">Effort</label>
            <select
              className="form-input"
              value={effort}
              onChange={(e) => setEffort(e.target.value)}
            >
              {effortOptions.map((o) => (
                <option key={o.value} value={o.value}>
                  {o.label}
                </option>
              ))}
            </select>
          </div>
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
          {dependencyCandidates.length > 0 && (
            <div className="form-field">
              <label className="form-label">Depends On</label>
              <p className="form-hint" style={{ marginTop: 0, marginBottom: 6 }}>
                A worker only starts this card once every selected card is done.
              </p>
              {selectedDependencies.length > 0 && (
                <ul className="dependency-chip-list">
                  {selectedDependencies.map((c) => (
                    <li key={c.id} className="dependency-chip">
                      <span className="dependency-chip-title">{c.title}</span>
                      <button
                        type="button"
                        className="dependency-chip-remove"
                        aria-label={`Remove dependency on ${c.title}`}
                        onClick={() => setDependsOn((prev) => prev.filter((id) => id !== c.id))}
                      >
                        ×
                      </button>
                    </li>
                  ))}
                </ul>
              )}
              <button
                type="button"
                className="btn-secondary dependency-picker-trigger"
                onClick={() => setPickerOpen(true)}
              >
                {selectedDependencies.length === 0
                  ? 'Select Dependencies...'
                  : `Edit Dependencies (${selectedDependencies.length})`}
              </button>
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
      </Modal>
      {pickerOpen && (
        <DependencyPickerModal
          candidates={dependencyCandidates}
          selectedIds={dependsOn}
          onCancel={() => setPickerOpen(false)}
          onConfirm={(ids) => {
            setDependsOn(ids)
            setPickerOpen(false)
          }}
        />
      )}
    </>
  )
}
