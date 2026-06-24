import { useEffect, useMemo, useState, type FormEvent } from 'react'
import type { RepeatingScheduleKind, RepeatingTask } from '../types/api'
import { useFoldersStore } from '../store/folders'
import { useRepeatingTasksStore } from '../store/repeatingTasks'
import { useResourcesStore, type ModelInfo } from '../store/resources'
import Modal from './Modal'
import ModelPicker from './ModelPicker'
import RepeatingTaskScheduleEditor from './RepeatingTaskScheduleEditor'

interface Props {
  initial?: RepeatingTask
  onClose: () => void
  onSaved?: (task: RepeatingTask) => void
}

const EFFORT_OPTIONS = [
  { value: '', label: 'Default' },
  { value: 'low', label: 'Low' },
  { value: 'medium', label: 'Medium' },
  { value: 'high', label: 'High' },
  { value: 'xhigh', label: 'Extra high' },
  { value: 'max', label: 'Max' },
]

export default function NewRepeatingTaskModal({ initial, onClose, onSaved }: Props) {
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)
  const createTask = useRepeatingTasksStore((s) => s.createTask)
  const updateTask = useRepeatingTasksStore((s) => s.updateTask)
  const models = useResourcesStore((s) => s.models)
  const fetchModels = useResourcesStore((s) => s.fetchModels)

  const editing = !!initial

  const initialScheduleValue = useMemo(() => {
    if (!initial) return { minutes: 60 } as Record<string, number>
    try {
      return JSON.parse(initial.schedule_value) as Record<string, number>
    } catch {
      return { minutes: 60 } as Record<string, number>
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
  const [scheduleValue, setScheduleValue] = useState<Record<string, number>>(initialScheduleValue)
  const [model, setModel] = useState<string>(initial?.model ?? '')
  const [effort, setEffort] = useState<string>(initial?.effort ?? '')
  const [enabled, setEnabled] = useState<boolean>(initial?.enabled ?? true)
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)

  useEffect(() => {
    fetchFolders()
    fetchModels()
  }, [fetchFolders, fetchModels])

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
    setLoading(true)
    setError('')
    try {
      if (editing && initial) {
        const task = await updateTask(initial.id, {
          name: name.trim(),
          description,
          prompt,
          schedule_kind: scheduleKind,
          schedule_value: scheduleValue,
          model: model || null,
          effort: effort || null,
          enabled,
        })
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
          <label className="form-label">Name</label>
          <input
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
          <label className="form-label">
            Description <span className="optional">(optional)</span>
          </label>
          <textarea
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
          <label className="form-label">Folder</label>
          {editing ? (
            <p className="form-help">{folders.find((f) => f.id === folderId)?.name ?? folderId}</p>
          ) : folders.length > 0 ? (
            <select
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
          <label className="form-label">Prompt</label>
          <textarea
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
        />

        <div className="form-field">
          <label className="form-label" htmlFor="repeating-task-model">
            Model
          </label>
          <ModelPicker
            id="repeating-task-model"
            value={model}
            onChange={setModel}
            models={models as ModelInfo[]}
            testId="repeating-task-model"
          />
          <p className="form-help">
            Each spawned run starts on this model. &quot;Default&quot; uses the system default.
          </p>
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
            {EFFORT_OPTIONS.map((o) => (
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
          <button type="button" className="btn-secondary" onClick={onClose}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={loading || !name.trim() || !folderId || !prompt.trim()}
          >
            {loading ? 'Saving...' : editing ? 'Save' : 'Create Task'}
          </button>
        </div>
      </form>
    </Modal>
  )
}
