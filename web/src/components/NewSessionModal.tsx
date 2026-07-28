import { useEffect, useMemo, useState, type FormEvent } from 'react'
import { useSessionsStore } from '../store/sessions'
import { authedFetch } from '../store/auth'
import { useFoldersStore } from '../store/folders'
import { effortOptionsForModel, useResourcesStore } from '../store/resources'
import Modal from './Modal'
import ModelPicker from './ModelPicker'
import SystemPromptPicker from './SystemPromptPicker'
import { PRESET_PROMPTS, presetSessionName } from '../utils/presetPrompts'
import type { WasmPlugin } from '../utils/pluginApproval'

interface Props {
  onClose: () => void
  /** Called with the new session's id once it exists. The host owns
   *  activation (select + navigate + open tab) so creating a session from a
   *  full-page view — Settings, Reports, Usage, … — lands the user in the
   *  new session instead of leaving the overlay up with an unselected tab. */
  onCreated: (sessionId: string) => void
}

export default function NewSessionModal({ onClose, onCreated }: Props) {
  const createSession = useSessionsStore((s) => s.createSession)
  const setDraft = useSessionsStore((s) => s.setDraft)
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)
  const createFolder = useFoldersStore((s) => s.createFolder)
  const models = useResourcesStore((s) => s.models)
  const providers = useResourcesStore((s) => s.providers)
  const fetchModels = useResourcesStore((s) => s.fetchModels)

  const [name, setName] = useState('')
  // Same derived-default pattern as NewProjectModal — see comment there.
  const [chosenFolderId, setChosenFolderId] = useState<string | null>(null)
  const folderId = chosenFolderId ?? folders[0]?.id ?? ''
  const [model, setModel] = useState('')
  const [effort, setEffort] = useState('')
  // Chat sessions default OFF (workers default ON); a NULL column inherits
  // that, so an unchecked box just leaves auto-switch off.
  const [modelAutoswitch, setModelAutoswitch] = useState(false)
  // Temp sessions are deleted server-side when their last tab is closed.
  const [isTemp, setIsTemp] = useState(false)
  const [systemPromptName, setSystemPromptName] = useState<string | null>(null)
  // '' = no preset (default). Picking one auto-sends its prompt as the
  // session's first message right after create.
  const [presetId, setPresetId] = useState('')
  const [topic, setTopic] = useState('')
  // True once /api/plugins confirms the playwright-video plugin is active —
  // gates the browser bug-hunt preset on its replay UI existing.
  const [playwrightVideoActive, setPlaywrightVideoActive] = useState(false)
  const [newFolderName, setNewFolderName] = useState('')
  const [newFolderPath, setNewFolderPath] = useState('')
  const [showNewFolder, setShowNewFolder] = useState(false)
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)

  useEffect(() => {
    fetchFolders()
  }, [fetchFolders])

  useEffect(() => {
    fetchModels()
  }, [fetchModels])

  // One-shot probe for the playwright-video plugin. Failures just leave
  // the bug-hunt preset hidden.
  useEffect(() => {
    let cancelled = false
    authedFetch('/api/plugins')
      .then((res) => (res.ok ? res.json() : null))
      .then((body: { wasm_plugins?: WasmPlugin[] } | null) => {
        const active = (body?.wasm_plugins ?? []).some(
          (p) => p.name === 'playwright-video' && p.status === 'approved',
        )
        if (!cancelled) setPlaywrightVideoActive(active)
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [])

  // Effort options follow the chosen model's provider.
  const effortOptions = useMemo(() => effortOptionsForModel(model, providers), [model, providers])
  // Clear a now-invalid effort back to Default on model change so we never
  // submit one the provider can't use.
  const handleModelChange = (id: string) => {
    setModel(id)
    const opts = effortOptionsForModel(id, providers)
    if (providers.length > 0 && effort && !opts.some((o) => o.value === effort)) setEffort('')
  }

  const availablePresets = PRESET_PROMPTS.filter(
    (p) => !p.requiresPlaywrightVideo || playwrightVideoActive,
  )
  const preset = availablePresets.find((p) => p.id === presetId)

  const handleCreateFolder = async () => {
    if (!newFolderName.trim() || !newFolderPath.trim()) return
    try {
      const folder = await createFolder(newFolderName.trim(), newFolderPath.trim())
      setChosenFolderId(folder.id)
      setShowNewFolder(false)
      setNewFolderName('')
      setNewFolderPath('')
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create folder')
    }
  }

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    if (!folderId || (!isTemp && !preset && !name.trim())) {
      setError('Name and folder are required')
      return
    }
    if (preset?.needsTopic && !topic.trim()) {
      setError(`${preset.topicLabel ?? 'Topic'} is required`)
      return
    }
    setLoading(true)
    setError('')
    try {
      // Name field is hidden for temp sessions; presets auto-name either way.
      const finalName = (isTemp ? '' : name.trim()) || presetSessionName(preset, topic)
      const session = await createSession(
        finalName,
        folderId,
        model || undefined,
        effort || undefined,
        modelAutoswitch,
        systemPromptName,
        isTemp,
      )
      if (preset) {
        // Same create-then-message pattern as utils/installSession.ts. The
        // session already exists here, so a failed send must not fail the
        // modal (retrying would duplicate the session) — park the prompt in
        // the input draft instead so nothing is lost.
        const text = preset.build(topic.trim())
        try {
          const msg = await authedFetch(`/api/sessions/${session.id}/message`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ text }),
          })
          if (!msg.ok) setDraft(session.id, text)
        } catch {
          setDraft(session.id, text)
        }
      }
      onCreated(session.id)
      onClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create session')
    } finally {
      setLoading(false)
    }
  }

  return (
    <Modal onClose={onClose}>
      <h2>New Session</h2>
      <form onSubmit={handleSubmit}>
        {/* Temp sessions are auto-named — the field would be dead weight. */}
        {!isTemp && (
          <div className="form-field">
            <label className="form-label" htmlFor="new-session-name">
              Name
            </label>
            <input
              id="new-session-name"
              className="form-input"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder={preset ? presetSessionName(preset, topic) : 'My session'}
              autoFocus
              required={!preset}
            />
          </div>
        )}
        <div className="form-field">
          <label className="form-label" htmlFor="new-session-folder">
            Folder
          </label>
          {folders.length > 0 ? (
            <select
              id="new-session-folder"
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
            <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text3)' }}>
              No folders yet. Create one below.
            </p>
          )}
          <button
            type="button"
            className="form-link-btn"
            onClick={() => setShowNewFolder(!showNewFolder)}
          >
            {showNewFolder ? 'Cancel' : '+ Add folder'}
          </button>
        </div>
        {showNewFolder && (
          <div className="form-inline-card">
            <input
              aria-label="New folder name"
              className="form-input"
              placeholder="Folder name"
              value={newFolderName}
              onChange={(e) => setNewFolderName(e.target.value)}
            />
            <input
              aria-label="New folder path"
              className="form-input"
              placeholder="/path/to/folder"
              value={newFolderPath}
              onChange={(e) => setNewFolderPath(e.target.value)}
            />
            <button
              type="button"
              className="btn-secondary"
              onClick={handleCreateFolder}
              disabled={!newFolderName.trim() || !newFolderPath.trim()}
            >
              Create Folder
            </button>
          </div>
        )}
        <div className="form-field">
          <label className="form-label" htmlFor="new-session-model">
            Model
          </label>
          <ModelPicker
            id="new-session-model"
            value={model}
            onChange={handleModelChange}
            models={models}
            defaultLabel="Auto"
            ariaLabel="Select model"
            testId="new-session-model"
          />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="new-session-effort">
            Effort
          </label>
          <select
            id="new-session-effort"
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
          <label className="form-label" htmlFor="new-session-system-prompt">
            System prompt
          </label>
          <SystemPromptPicker
            id="new-session-system-prompt"
            value={systemPromptName}
            onChange={setSystemPromptName}
            testId="new-session-system-prompt"
          />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="new-session-preset">
            Preset prompt
          </label>
          <select
            id="new-session-preset"
            className="form-input"
            value={presetId}
            onChange={(e) => setPresetId(e.target.value)}
            data-testid="new-session-preset"
          >
            <option value="">None — start empty</option>
            {availablePresets.map((p) => (
              <option key={p.id} value={p.id}>
                {p.label}
              </option>
            ))}
          </select>
        </div>
        {preset?.needsTopic && (
          <div className="form-field">
            <label className="form-label" htmlFor="new-session-preset-topic">
              {preset.topicLabel ?? 'Topic'}
            </label>
            <input
              id="new-session-preset-topic"
              className="form-input"
              value={topic}
              onChange={(e) => setTopic(e.target.value)}
              placeholder={preset.topicPlaceholder}
              data-testid="new-session-preset-topic"
              required
            />
          </div>
        )}
        <div className="form-field">
          <label className="form-checkbox-label">
            <input
              type="checkbox"
              checked={modelAutoswitch}
              onChange={(e) => setModelAutoswitch(e.target.checked)}
              data-testid="new-session-autoswitch"
            />
            <span>Allow auto-switching to a cheaper model</span>
          </label>
        </div>
        <div className="form-field">
          <label className="form-checkbox-label">
            <input
              type="checkbox"
              checked={isTemp}
              onChange={(e) => setIsTemp(e.target.checked)}
              data-testid="new-session-temp"
            />
            <span>Temporary — delete this session when its tab is closed</span>
          </label>
        </div>
        {error && <p className="form-error">{error}</p>}
        <div className="form-actions">
          <button type="button" className="btn-secondary" onClick={onClose}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={
              loading ||
              !folderId ||
              (!isTemp && !preset && !name.trim()) ||
              (!!preset?.needsTopic && !topic.trim())
            }
          >
            {loading ? 'Creating...' : 'Create Session'}
          </button>
        </div>
      </form>
    </Modal>
  )
}
