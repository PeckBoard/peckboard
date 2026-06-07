import { useEffect, useState, type FormEvent } from 'react'
import { useProjectsStore } from '../store/projects'
import { authedFetch } from '../store/auth'
import type { Card } from '../types/api'

interface Props {
  projectId: string
  card: Card
  onClose: () => void
}

interface WorkflowInfo {
  id: string
  name: string
}
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

export default function EditCardModal({ projectId, card, onClose }: Props) {
  const updateCard = useProjectsStore((s) => s.updateCard)

  const isTerminal = card.step === 'done' || card.step === 'wont_do'
  const isBacklog = card.step === 'backlog'

  const [title, setTitle] = useState(card.title)
  const [description, setDescription] = useState(card.description)
  const [priority, setPriority] = useState(card.priority)
  const [workflow, setWorkflow] = useState(card.workflow ?? '')
  const [model, setModel] = useState(card.model ?? '')
  const [effort, setEffort] = useState(card.effort ?? '')
  const [blocked, setBlocked] = useState(card.blocked)
  const [blockReason, setBlockReason] = useState(card.block_reason ?? '')
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)

  const [workflows, setWorkflows] = useState<WorkflowInfo[]>([])
  const [models, setModels] = useState<ModelInfo[]>([])
  const [priorities, setPriorities] = useState<{ label: string; value: number }[]>([
    { label: 'Critical', value: 0 }, { label: 'High', value: 1 },
    { label: 'Medium', value: 2 }, { label: 'Low', value: 3 }, { label: 'Backlog', value: 4 },
  ])

  useEffect(() => {
    authedFetch('/api/priorities')
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => { if (data?.priorities) setPriorities(data.priorities) })
      .catch(() => {})
    authedFetch('/api/workflows')
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (data?.workflows) setWorkflows(data.workflows)
      })
      .catch(() => {})
    authedFetch('/api/models')
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (data?.models) setModels(data.models)
      })
      .catch(() => {})
  }, [])

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    if (!title.trim()) {
      setError('Title is required')
      return
    }
    setLoading(true)
    setError('')
    try {
      const updates: Partial<Card> = {
        title: title.trim(),
        priority,
        blocked,
        block_reason: blocked ? blockReason.trim() || null : null,
        model: model || null,
        effort: effort || null,
      }
      // Description and workflow are backlog-only fields
      if (isBacklog) {
        updates.description = description.trim()
        updates.workflow = workflow || null
      }
      await updateCard(projectId, card.id, updates)
      onClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to update card')
    } finally {
      setLoading(false)
    }
  }

  if (isTerminal) {
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
              <span>{card.title}</span>
            </div>
            <div className="card-detail-row">
              <span className="card-detail-label">Step</span>
              <span>{card.step}</span>
            </div>
            {card.description && (
              <div className="card-detail-row">
                <span className="card-detail-label">Description</span>
                <span>{card.description}</span>
              </div>
            )}
            {card.workflow && (
              <div className="card-detail-row">
                <span className="card-detail-label">Workflow</span>
                <span>{card.workflow}</span>
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
      <div className="modal" onClick={(e) => e.stopPropagation()} style={{ maxWidth: 480 }}>
        <h2>Edit Card</h2>
        <form onSubmit={handleSubmit}>
          <div className="form-field">
            <label className="form-label">Title</label>
            <input
              className="form-input"
              value={title}
              onChange={(e) => setTitle(e.target.value)}
              required
            />
          </div>
          <div className="form-field">
            <label className="form-label">
              Description {!isBacklog && <span className="optional">(locked)</span>}
            </label>
            <textarea
              className="form-input"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              rows={3}
              style={{ resize: 'vertical' }}
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
                <option key={p.value} value={p.value}>{p.label}</option>
              ))}
            </select>
          </div>
          <div className="form-field">
            <label className="form-label">
              Workflow {!isBacklog && <span className="optional">(locked)</span>}
            </label>
            <select
              className="form-input"
              value={workflow}
              onChange={(e) => setWorkflow(e.target.value)}
              disabled={!isBacklog}
            >
              <option value="">Default</option>
              {workflows.map((w) => (
                <option key={w.id} value={w.id}>
                  {w.name}
                </option>
              ))}
            </select>
          </div>
          <div className="form-field">
            <label className="form-label">Model</label>
            <select className="form-input" value={model} onChange={(e) => setModel(e.target.value)}>
              <option value="">Default</option>
              {models.map((m) => (
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
          {error && <p className="form-error">{error}</p>}
          <div className="form-actions">
            <button type="button" className="btn-secondary" onClick={onClose}>
              Cancel
            </button>
            <button type="submit" className="btn-primary" disabled={loading || !title.trim()}>
              {loading ? 'Saving...' : 'Save'}
            </button>
          </div>
        </form>
      </div>
    </div>
  )
}
