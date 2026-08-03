import { useEffect, useMemo, useState, type FormEvent } from 'react'
import type { RepeatingScheduleKind, RepeatingTask } from '../types/api'
import { useFoldersStore } from '../store/folders'
import { useRepeatingTasksStore, type UpdateRepeatingTaskInput } from '../store/repeatingTasks'
import {
  effortOptionsForModel,
  modelGoneFromCatalogue,
  useResourcesStore,
  type ModelInfo,
} from '../store/resources'
import Modal from './Modal'
import ModelPicker from './ModelPicker'
import ModelGoneNotice from './ModelGoneNotice'
import RepeatingTaskScheduleEditor from './RepeatingTaskScheduleEditor'
import { scheduleProblem } from '../utils/repeatingSchedule'

interface Props {
  initial?: RepeatingTask
  onClose: () => void
  onSaved?: (task: RepeatingTask) => void
}

export default function NewRepeatingTaskModal({ initial, onClose, onSaved }: Props) {
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)
  const createTask = useRepeatingTasksStore((s) => s.createTask)
  const updateTask = useRepeatingTasksStore((s) => s.updateTask)
  const models = useResourcesStore((s) => s.models)
  const providers = useResourcesStore((s) => s.providers)
  const fetchModels = useResourcesStore((s) => s.fetchModels)
  const defaultModel = useResourcesStore((s) => s.defaultModel)
  const fetchDefaultModel = useResourcesStore((s) => s.fetchDefaultModel)

  const editing = !!initial

  const initialScheduleValue = useMemo(() => {
    if (!initial) return { minutes: 60 } as Record<string, number | string>
    try {
      return JSON.parse(initial.schedule_value) as Record<string, number | string>
    } catch {
      return { minutes: 60 } as Record<string, number | string>
    }
  }, [initial])

  const [name, setName] = useState(initial?.name ?? '')
  const [description, setDescription] = useState(initial?.description ?? '')
  const [chosenFolderId, setChosenFolderId] = useState<string | null>(initial?.folder_id ?? null)
  const folderId = chosenFolderId ?? initial?.folder_id ?? folders[0]?.id ?? ''
  const [prompt, setPrompt] = useState(initial?.prompt ?? '')
  const [scheduleKind, setScheduleKind] = useState<RepeatingScheduleKind>(
    initial?.schedule_kind ?? 'interval',
  )
  const [scheduleValue, setScheduleValue] =
    useState<Record<string, number | string>>(initialScheduleValue)
  const [timezone, setTimezone] = useState<string>(initial?.timezone ?? '')
  const [chosenModel, setChosenModel] = useState<string | null>(null)
  // The app-wide default model (Settings → Default Model) preselects — for
  // a new task and for a legacy task saved without one — unless it's gone
  // from the catalogue (provider removed): a dead id must not preselect,
  // an untouched save would pin it. Treat it as unset and warn instead.
  const defaultModelGone = modelGoneFromCatalogue(defaultModel, models)
  const model = chosenModel ?? initial?.model ?? (defaultModelGone ? '' : defaultModel)
  const [effort, setEffort] = useState<string>(initial?.effort ?? '')
  // Effort options follow the chosen model's provider.
  const effortOptions = useMemo(() => effortOptionsForModel(model, providers), [model, providers])
  // Clear a now-invalid effort back to Default on model change so we never
  // save one the provider can't use.
  const handleModelChange = (id: string) => {
    setChosenModel(id)
    const opts = effortOptionsForModel(id, providers)
    if (providers.length > 0 && effort && !opts.some((o) => o.value === effort)) setEffort('')
  }
  const [enabled, setEnabled] = useState<boolean>(initial?.enabled ?? true)
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)

  useEffect(() => {
    fetchFolders()
    fetchModels()
    fetchDefaultModel()
  }, [fetchFolders, fetchModels, fetchDefaultModel])

  // The requirement the form doesn't meet yet. Shown next to the disabled
  // primary action so a blocked Create never leaves the user guessing.
  const scheduleIssue = scheduleProblem(scheduleKind, scheduleValue)
  const disabledReason = !name.trim()
    ? 'Enter a name'
    : !folderId
      ? 'Add a folder first'
      : !prompt.trim()
        ? 'Enter a prompt'
        : scheduleIssue

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    if (!name.trim()) {
      setError('Name is required')
      return
    }
    if (!folderId) {
      setError('Folder is required')
      return
    }
    if (!prompt.trim()) {
      setError('Prompt is required')
      return
    }
    if (scheduleIssue) {
      setError(scheduleIssue)
      return
    }
    setLoading(true)
    setError('')
    try {
      if (editing && initial) {
        const updates: UpdateRepeatingTaskInput = {
          name: name.trim(),
          description,
          prompt,
          schedule_kind: scheduleKind,
          schedule_value: scheduleValue,
          effort: effort || null,
          enabled,
          timezone: timezone || null,
        }
        // Only write `model` when the user actually opened the picker —
        // same rule as CardFormModal. Sending it unconditionally would
        // materialize the displayed default onto a legacy no-model task on
        // any save; an omitted key leaves the stored value untouched.
        if (chosenModel !== null) updates.model = chosenModel || null
        const task = await updateTask(initial.id, updates)
        onSaved?.(task)
      } else {
        const task = await createTask({
          name: name.trim(),
          description,
          folder_id: folderId,
          prompt,
          schedule_kind: scheduleKind,
          schedule_value: scheduleValue,
          model: model || null,
          effort: effort || null,
          enabled,
          timezone: timezone || null,
        })
        onSaved?.(task)
      }
      onClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to save task')
    } finally {
      setLoading(false)
    }
  }

  return (
    <Modal onClose={onClose} maxWidth={560}>
      <h2>{editing ? 'Edit Repeating Task' : 'New Repeating Task'}</h2>
      <form onSubmit={handleSubmit}>
        <div className="form-field">
          <label className="form-label" htmlFor="repeating-task-name">
            Name
          </label>
          <input
            id="repeating-task-name"
            className="form-input"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="Daily project sweep"
            autoFocus
            required
            maxLength={200}
          />
        </div>

        <div className="form-field">
          <label className="form-label" htmlFor="repeating-task-description">
            Description <span className="optional">(optional)</span>
          </label>
          <textarea
            id="repeating-task-description"
            className="form-input"
            value={description}
            onChange={(e) => setDescription(e.target.value)}
            placeholder="What this task is for. Shown in the list; not sent to the agent."
            rows={2}
            style={{ resize: 'vertical' }}
            maxLength={2000}
          />
        </div>

        <div className="form-field">
          <label className="form-label" htmlFor="repeating-task-folder">
            Folder
          </label>
          {editing ? (
            <p className="form-help">{folders.find((f) => f.id === folderId)?.name ?? folderId}</p>
          ) : folders.length > 0 ? (
            <select
              id="repeating-task-folder"
              className="form-input"
              value={folderId}
              onChange={(e) => setChosenFolderId(e.target.value)}
            >
              {folders.map((f) => (
                <option key={f.id} value={f.id}>
                  {f.name} — {f.path}
                </option>
              ))}
            </select>
          ) : (
            <p className="form-help">No folders yet. Create one from the folder manager first.</p>
          )}
        </div>

        <div className="form-field">
          <label className="form-label" htmlFor="repeating-task-prompt">
            Prompt
          </label>
          <textarea
            id="repeating-task-prompt"
            className="form-input"
            value={prompt}
            onChange={(e) => setPrompt(e.target.value)}
            placeholder="The message sent to the new session on each run."
            rows={6}
            style={{ resize: 'vertical' }}
            required
          />
        </div>

        <RepeatingTaskScheduleEditor
          kind={scheduleKind}
          value={scheduleValue}
          onChange={(k, v) => {
            setScheduleKind(k)
            setScheduleValue(v)
          }}
          timezone={timezone}
          onTimezoneChange={setTimezone}
        />

        <div className="form-field">
          <label className="form-label" htmlFor="repeating-task-model">
            Model
          </label>
          <ModelPicker
            id="repeating-task-model"
            value={model}
            onChange={handleModelChange}
            models={models as ModelInfo[]}
            testId="repeating-task-model"
          />
          <p className="form-help">Each spawned run starts on this model.</p>
          {chosenModel === null && !initial?.model && defaultModelGone && (
            <ModelGoneNotice modelId={defaultModel} />
          )}
        </div>

        <div className="form-field">
          <label className="form-label" htmlFor="repeating-task-effort">
            Effort
          </label>
          <select
            id="repeating-task-effort"
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
          <label className="form-label" style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
            <input
              type="checkbox"
              checked={enabled}
              onChange={(e) => setEnabled(e.target.checked)}
            />
            <span>Enabled</span>
          </label>
          <p className="form-help">
            When off, the scheduler won&apos;t fire this task. You can still trigger it manually
            with &quot;Run now&quot;.
          </p>
        </div>

        {error && <p className="form-error">{error}</p>}
        <div className="form-actions">
          {!loading && disabledReason && (
            <span className="form-actions-reason" data-testid="repeating-task-disabled-reason">
              {disabledReason}
            </span>
          )}
          <button type="button" className="btn-secondary" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="btn-primary" disabled={loading || !!disabledReason}>
            {loading ? 'Saving...' : editing ? 'Save' : 'Create Task'}
          </button>
        </div>
      </form>
    </Modal>
  )
}
