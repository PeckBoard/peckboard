import { useEffect, useMemo, useState, type FormEvent } from 'react'
import { useProjectsStore } from '../store/projects'
import {
  effortOptionsForModel,
  modelGoneFromCatalogue,
  useResourcesStore,
} from '../store/resources'
import { authedFetch } from '../store/auth'
import type { Card } from '../types/api'
import DependencyPickerModal from './DependencyPickerModal'
import Modal from './Modal'
import ModelPicker from './ModelPicker'
import ModelGoneNotice from './ModelGoneNotice'
import PickerLoadError from './PickerLoadError'
import SystemPromptPicker from './SystemPromptPicker'
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
  const [chosenModel, setChosenModel] = useState<string | null>(null)
  const [effort, setEffort] = useState(card?.effort ?? '')
  const [blocked, setBlocked] = useState(card?.blocked ?? false)
  const [blockReason, setBlockReason] = useState(card?.block_reason ?? '')
  const [modelAutoswitch, setModelAutoswitch] = useState(card?.model_autoswitch ?? true)
  const [systemPromptName, setSystemPromptName] = useState<string | null>(
    card?.system_prompt_name ?? null,
  )
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
  const modelsLoadError = useResourcesStore((s) => s.resourceErrors.models)
  const defaultModel = useResourcesStore((s) => s.defaultModel)
  const fetchDefaultModel = useResourcesStore((s) => s.fetchDefaultModel)
  // A card's model is an inherit chain: card → step → project → app-wide
  // default (Settings → Default Model). A new card preselects the app
  // default; an existing card shows its own pin, or the inherit row when it
  // has none — materializing the default into the value here would silently
  // pin it onto an inherit card the next time any field is saved.
  // A gone default (provider removed) must not preselect on create either —
  // an untouched form would send it as an explicit dead pin. Unset + warn.
  const defaultModelGone = modelGoneFromCatalogue(defaultModel, models)
  const model =
    chosenModel ?? card?.model ?? (mode === 'create' && !defaultModelGone ? defaultModel : '')
  // What an unpinned card actually dispatches workers on, so the inherit row
  // names the effective model instead of leaving the user guessing.
  const inheritedModel = project?.model ?? defaultModel
  const inheritedLabel = models.find((m) => m.id === inheritedModel)?.display_name ?? inheritedModel
  const inheritLabel = inheritedModel ? `Default (${inheritedLabel})` : 'Default'
  const [priorities, setPriorities] = useState<{ label: string; value: number }[]>([
    { label: 'Critical', value: 0 },
    { label: 'High', value: 1 },
    { label: 'Medium', value: 2 },
    { label: 'Low', value: 3 },
  ])

  useEffect(() => {
    fetchWorkflows()
    fetchModels()
    fetchDefaultModel()
    authedFetch('/api/priorities')
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (data?.priorities) setPriorities(data.priorities)
      })
      .catch(() => {})
  }, [fetchWorkflows, fetchModels, fetchDefaultModel])

  // Effort options follow the effective model's provider — for an inherit
  // card that is the inherited model, not an empty id.
  const effortOptions = useMemo(
    () => effortOptionsForModel(model || inheritedModel, providers),
    [model, inheritedModel, providers],
  )
  // Clear a now-invalid effort back to Default on model change so we never
  // save one the provider can't use.
  const handleModelChange = (id: string) => {
    setChosenModel(id)
    const opts = effortOptionsForModel(id || inheritedModel, providers)
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
          model_autoswitch: modelAutoswitch,
          system_prompt_name: systemPromptName || null,
        } as Partial<Card>)
      } else {
        const updates: Partial<Card> = {
          title: title.trim(),
          priority,
          blocked,
          block_reason: blocked ? blockReason.trim() || null : null,
          effort: effort || null,
          depends_on: dependsOn,
          model_autoswitch: modelAutoswitch,
          system_prompt_name: systemPromptName ?? '',
        }
        // Only write `model` when the user actually opened the picker.
        // Sending it unconditionally would pin whatever the form displays
        // onto a card that deliberately inherits; an omitted key leaves the
        // stored value untouched, an explicit null un-pins.
        if (chosenModel !== null) updates.model = chosenModel || null
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
            <label className="form-label" htmlFor="card-title">
              Title
            </label>
            <input
              id="card-title"
              className="form-input"
              value={title}
              onChange={(e) => setTitle(e.target.value)}
              autoFocus={mode === 'create'}
              required
            />
          </div>
          <div className="form-field">
            <label className="form-label" htmlFor="card-description">
              Description {!isBacklog && <span className="optional">(locked)</span>}
            </label>
            <textarea
              id="card-description"
              className="form-input card-form-description"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              rows={8}
              disabled={!isBacklog}
            />
          </div>
          <div className="form-field">
            <label className="form-label" htmlFor="card-priority">
              Priority
            </label>
            <select
              id="card-priority"
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
            <label className="form-label" htmlFor="card-workflow">
              Workflow {!isBacklog && <span className="optional">(locked)</span>}
            </label>
            <WorkflowSelect
              id="card-workflow"
              value={workflow}
              onChange={setWorkflow}
              projectWorkflowId={projectWorkflowId ?? undefined}
              projectWorkflowName={projectWorkflowName}
              disabled={!isBacklog}
            />
          </div>
          <div className="form-field">
            <label className="form-label" htmlFor="card-model">
              Model
            </label>
            <ModelPicker
              id="card-model"
              value={model}
              onChange={handleModelChange}
              models={models as ModelInfo[]}
              defaultLabel={inheritLabel}
              testId="card-model"
            />
            {modelsLoadError && models.length === 0 && (
              <PickerLoadError label="models" onRetry={fetchModels} />
            )}
            {mode === 'create' && chosenModel === null && defaultModelGone && (
              <ModelGoneNotice modelId={defaultModel} />
            )}
          </div>
          <div className="form-field">
            <label className="form-label" htmlFor="card-system-prompt">
              System prompt
            </label>
            <SystemPromptPicker
              id="card-system-prompt"
              value={systemPromptName}
              onChange={setSystemPromptName}
              testId="card-system-prompt"
            />
          </div>
          <div className="form-field">
            <label className="form-label" htmlFor="card-effort">
              Effort
            </label>
            <select
              id="card-effort"
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
                checked={modelAutoswitch}
                onChange={(e) => setModelAutoswitch(e.target.checked)}
                data-testid="card-autoswitch"
              />
              <span>Auto-switch to a cheaper model when the plan allows</span>
            </label>
            <p className="form-hint" style={{ marginTop: 4 }}>
              The worker plans first on the configured model, then may downgrade itself for simple
              work.
            </p>
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
                aria-label="Block reason"
                placeholder="Block reason..."
                value={blockReason}
                onChange={(e) => setBlockReason(e.target.value)}
              />
            )}
          </div>
          {dependencyCandidates.length > 0 && (
            <div className="form-field">
              <span className="form-label">Depends On</span>
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
            {!loading && !title.trim() && (
              <span className="form-actions-reason" data-testid="card-form-disabled-reason">
                Enter a title
              </span>
            )}
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
