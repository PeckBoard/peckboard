import { useEffect, useMemo, useState, type FormEvent } from 'react'
import HelpTip from './HelpTip'
import { useSessionsStore } from '../store/sessions'
import { authedFetch, useAuthStore } from '../store/auth'
import { useFoldersStore } from '../store/folders'
import {
  effortOptionsForModel,
  modelGoneFromCatalogue,
  useResourcesStore,
} from '../store/resources'
import Modal from './Modal'
import NewFolderModal from './NewFolderModal'
import ModelPicker from './ModelPicker'
import ModelGoneNotice from './ModelGoneNotice'
import SystemPromptPicker from './SystemPromptPicker'
import PickerLoadError from './PickerLoadError'
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
  // POST /api/folders is admin-only, so only admins get the New Folder entry.
  const isAdmin = useAuthStore((s) => s.user?.role === 'admin')
  const models = useResourcesStore((s) => s.models)
  const providers = useResourcesStore((s) => s.providers)
  const fetchModels = useResourcesStore((s) => s.fetchModels)
  const modelsLoadError = useResourcesStore((s) => s.resourceErrors.models)
  const defaultModel = useResourcesStore((s) => s.defaultModel)
  const fetchDefaultModel = useResourcesStore((s) => s.fetchDefaultModel)

  const [name, setName] = useState('')
  const sessions = useSessionsStore((s) => s.sessions)
  // Until the user picks, default to the folder of the most recently active
  // chat (when it still exists), else the first — usually only — folder.
  const [chosenFolderId, setChosenFolderId] = useState<string | null>(null)
  const recentFolderId = useMemo(() => {
    let best: { folderId: string; at: string } | null = null
    for (const s of sessions) {
      if (s.is_worker || !folders.some((f) => f.id === s.folder_id)) continue
      if (!best || s.last_activity > best.at) best = { folderId: s.folder_id, at: s.last_activity }
    }
    return best?.folderId ?? null
  }, [sessions, folders])
  const folderId = chosenFolderId ?? recentFolderId ?? folders[0]?.id ?? ''
  // Same derived-default pattern as the folder above: the app-wide default
  // model (Settings → Default Model) preselects until the user picks one.
  const [chosenModel, setChosenModel] = useState<string | null>(null)
  // …unless that default's provider is gone (ollama rm, plugin uninstall):
  // preselecting the dead id would submit it as an explicit dead pin from
  // an untouched form. Treat it as unset and warn instead.
  const defaultModelGone = modelGoneFromCatalogue(defaultModel, models)
  const model = chosenModel ?? (defaultModelGone ? '' : defaultModel)
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
  const [showNewFolder, setShowNewFolder] = useState(false)
  // Rarely-needed settings sit behind "More options". A fresh form starts
  // with every one of them at its default, so it starts closed.
  const [showMore, setShowMore] = useState(false)
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)

  useEffect(() => {
    fetchFolders()
  }, [fetchFolders])
  // With no folders, an admin gets a one-click ready-made folder: the same
  // `~/PeckBoard` default the setup wizard offers (GET /api/folders/home).
  const createFolder = useFoldersStore((s) => s.createFolder)
  const [homeSuggested, setHomeSuggested] = useState<string | null>(null)
  const [homeBusy, setHomeBusy] = useState(false)
  const noFolders = folders.length === 0
  useEffect(() => {
    if (!isAdmin || !noFolders) return
    let cancelled = false
    authedFetch('/api/folders/home')
      .then((res) => (res.ok ? res.json() : null))
      .then((body: { suggested?: string | null } | null) => {
        if (!cancelled) setHomeSuggested(body?.suggested ?? null)
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [isAdmin, noFolders])
  const createHomeFolder = async (path: string) => {
    setHomeBusy(true)
    setError('')
    try {
      const folder = await createFolder('My work', path, true)
      setChosenFolderId(folder.id)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create folder')
    } finally {
      setHomeBusy(false)
    }
  }

  // Local "the catalogue has answered" flag, so an empty list mid-fetch
  // isn't mistaken for "no AI connected".
  const [modelsChecked, setModelsChecked] = useState(false)
  useEffect(() => {
    fetchModels().finally(() => setModelsChecked(true))
  }, [fetchModels])
  const noModels = modelsChecked && !modelsLoadError && models.length === 0
  useEffect(() => {
    fetchDefaultModel()
  }, [fetchDefaultModel])

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
    setChosenModel(id)
    const opts = effortOptionsForModel(id, providers)
    if (providers.length > 0 && effort && !opts.some((o) => o.value === effort)) setEffort('')
  }

  const availablePresets = PRESET_PROMPTS.filter(
    (p) => !p.requiresPlaywrightVideo || playwrightVideoActive,
  )
  const preset = availablePresets.find((p) => p.id === presetId)

  // Why Create is disabled, shown next to the button — a disabled control
  // with no stated reason reads as broken.
  const disabledReason = !folderId
    ? folders.length === 0
      ? isAdmin
        ? 'Add a folder first'
        : 'Ask an admin to add a folder'
      : 'Choose a folder'
    : noModels
      ? 'Connect an AI first (Settings → Providers)'
      : preset?.needsTopic && !topic.trim()
        ? `Enter a ${(preset.topicLabel ?? 'topic').toLowerCase()}`
        : ''

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    if (disabledReason) {
      setError(disabledReason)
      return
    }
    setLoading(true)
    setError('')
    try {
      // Name field is hidden for temp sessions; an empty name auto-names
      // from the preset, or "New session" / "Temp session" without one.
      const finalName = (isTemp ? '' : name.trim()) || presetSessionName(preset, topic, isTemp)
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
              Name <span className="optional">(optional)</span>
            </label>
            <input
              id="new-session-name"
              className="form-input"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder={presetSessionName(preset, topic, false)}
              autoFocus
            />
          </div>
        )}
        <div className="form-field">
          <label className="form-label" htmlFor="new-session-folder">
            Folder <HelpTip term="folder" />
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
            <p className="form-hint" data-testid="new-session-no-folders">
              {isAdmin
                ? 'No folders yet. A session works inside a folder on this computer — add one to start.'
                : 'No folders yet. Ask an admin to add one.'}
            </p>
          )}
          {folders.length === 0 && isAdmin && homeSuggested && (
            <button
              type="button"
              className="btn-secondary btn-small"
              onClick={() => void createHomeFolder(homeSuggested)}
              disabled={homeBusy}
              data-testid="new-session-home-folder"
            >
              {homeBusy ? 'Creating…' : `Create a PeckBoard folder in your home (${homeSuggested})`}
            </button>
          )}
          {isAdmin && (
            <button
              type="button"
              className="form-link-btn"
              onClick={() => setShowNewFolder(true)}
              data-testid="new-session-new-folder"
            >
              + New Folder
            </button>
          )}
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="new-session-model">
            Model <HelpTip term="model" />
          </label>
          <ModelPicker
            id="new-session-model"
            value={model}
            onChange={handleModelChange}
            models={models}
            ariaLabel="Select model"
            testId="new-session-model"
          />
          {modelsLoadError && models.length === 0 && (
            <PickerLoadError label="models" onRetry={fetchModels} />
          )}
          {chosenModel === null && defaultModelGone && <ModelGoneNotice modelId={defaultModel} />}
        </div>
        {/* Outside the disclosure: a preset that needs a topic blocks Create
            until it's filled, so the field must stay visible. */}
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

        <button
          type="button"
          className="form-toggle-advanced"
          aria-expanded={showMore}
          onClick={() => setShowMore(!showMore)}
          data-testid="new-session-more-options"
        >
          {showMore ? 'Fewer options' : 'More options'}
        </button>

        {showMore && (
          <div className="form-advanced-section">
            <div className="form-field">
              <label className="form-label" htmlFor="new-session-effort">
                Effort <HelpTip term="effort" />
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
                Role and instructions <HelpTip term="systemPrompt" />
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
              <p className="form-hint">Sends a ready-made first message as soon as it opens.</p>
            </div>
            <div className="form-field">
              <label className="form-checkbox-label">
                <input
                  type="checkbox"
                  checked={modelAutoswitch}
                  onChange={(e) => setModelAutoswitch(e.target.checked)}
                  data-testid="new-session-autoswitch"
                />
                <span>Let PeckBoard use a cheaper model for simple replies</span>
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
          </div>
        )}
        {error && <p className="form-error">{error}</p>}
        <div className="form-actions">
          {!loading && disabledReason && (
            <span className="form-actions-reason" data-testid="new-session-disabled-reason">
              {disabledReason}
            </span>
          )}
          <button type="button" className="btn-secondary" onClick={onClose}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={loading || !!disabledReason}
            data-testid="new-session-create"
          >
            {loading ? 'Creating...' : 'Create Session'}
          </button>
        </div>
      </form>
      {/* Outside the <form>: React bubbles synthetic events through portals,
          so the nested dialog's submit would otherwise reach handleSubmit. */}
      {showNewFolder && (
        <NewFolderModal
          onClose={() => setShowNewFolder(false)}
          onCreated={(folder) => setChosenFolderId(folder.id)}
        />
      )}
    </Modal>
  )
}
