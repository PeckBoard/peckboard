import { useEffect, useState, type FormEvent } from 'react'
import {
  useVoiceStore,
  type AssistantDraft,
  type AssistantInfo,
  type AssistantTemplate,
} from '../../store/voice'
import { getSpeechEngine, type VoiceOption } from '../../voice/engine'
import { isKokoroVoice, isServerVoice, isStudioVoice } from '../../voice/kokoro'
import ConfirmDialog from '../ConfirmDialog'
import FieldError from '../FieldError'
import Modal from '../Modal'

const MAX_NAME = 24
const MAX_JOB = 4000

/** The colours an assistant can wear on its chip. */
const ASSISTANT_COLORS: { value: string; label: string }[] = [
  { value: '#3b82f6', label: 'Blue' },
  { value: '#10b981', label: 'Green' },
  { value: '#f59e0b', label: 'Amber' },
  { value: '#ef4444', label: 'Red' },
  { value: '#8b5cf6', label: 'Violet' },
  { value: '#ec4899', label: 'Pink' },
  { value: '#14b8a6', label: 'Teal' },
  { value: '#64748b', label: 'Slate' },
]

/** Same rules as the server (`db::crud::assistants::validate_name`): the
 *  name is spoken and matched against speech. */
function nameProblem(raw: string, others: AssistantInfo[]): string {
  const name = raw.trim().replace(/\s+/g, ' ')
  if (!name) return 'Give the assistant a name'
  if (name.length > MAX_NAME) return `Use ${MAX_NAME} characters or fewer`
  if (!/^\p{L}/u.test(name)) return 'Start the name with a letter'
  if (!/^[\p{L}\p{N} '-]+$/u.test(name)) return "Use only letters, digits, spaces, - and '"
  if (others.some((a) => a.name.toLowerCase() === name.toLowerCase()))
    return `Another assistant is already called ${name}`
  return ''
}

interface Props {
  /** The assistant to edit; `null` adds a new one. */
  assistant: AssistantInfo | null
  onClose: () => void
}

/**
 * Add or edit one assistant: its name, job, voice and colour. Adding starts
 * from a gallery of ready-made jobs; every field stays editable. Deleting
 * (never the default assistant) removes its conversation too.
 */
export default function AssistantDialog({ assistant, onClose }: Props) {
  const assistants = useVoiceStore((s) => s.assistants)
  const templates = useVoiceStore((s) => s.assistantTemplates)
  const saveAssistant = useVoiceStore((s) => s.saveAssistant)
  const deleteAssistant = useVoiceStore((s) => s.deleteAssistant)
  const prefs = useVoiceStore((s) => s.prefs)

  const [name, setName] = useState(assistant?.name ?? '')
  const [job, setJob] = useState(assistant?.job ?? '')
  const [voice, setVoice] = useState(assistant?.voice ?? '')
  const [color, setColor] = useState(assistant?.color ?? ASSISTANT_COLORS[0].value)
  const [template, setTemplate] = useState<string | null>(assistant?.template ?? null)
  const [makeDefault, setMakeDefault] = useState(false)
  const [touched, setTouched] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [deleteError, setDeleteError] = useState<string | null>(null)
  const [voices, setVoices] = useState<VoiceOption[]>(() => getSpeechEngine().getVoices())

  useEffect(() => {
    const engine = getSpeechEngine()
    return engine.onVoicesChanged(() => setVoices(engine.getVoices()))
  }, [])

  const others = assistants.filter((a) => a.id !== assistant?.id)
  const nameErr = nameProblem(name, others)
  const jobErr = job.length > MAX_JOB ? `Use ${MAX_JOB} characters or fewer` : ''
  const reason = nameErr || jobErr

  const applyTemplate = (t: AssistantTemplate) => {
    setTemplate(t.id)
    setJob(t.job)
    setColor(t.color)
    // Keep a name the user already typed; suggest one otherwise, unless taken.
    if (!name.trim() && !nameProblem(t.name, others)) setName(t.name)
  }

  const preview = () => {
    const engine = getSpeechEngine()
    engine.unlockSynthesis()
    engine.speak(`Hi, I'm ${name.trim() || 'your assistant'}.`, {
      voiceURI: voice || prefs.voiceURI,
      rate: prefs.rate,
      pitch: prefs.pitch,
      onError: () => {},
      onEnd: () => {},
    })
  }

  const submit = (e: FormEvent) => {
    e.preventDefault()
    setTouched(true)
    if (reason || busy) return
    setBusy(true)
    setError(null)
    const draft: AssistantDraft = {
      name: name.trim(),
      job: job.trim(),
      voice: voice || null,
      color,
      ...(assistant ? {} : { template }),
      ...(makeDefault ? { is_default: true } : {}),
    }
    void saveAssistant(assistant?.id ?? null, draft).then((err) => {
      setBusy(false)
      if (err) setError(err)
      else onClose()
    })
  }

  const remove = () => {
    if (!assistant) return
    setBusy(true)
    setDeleteError(null)
    void deleteAssistant(assistant.id).then((err) => {
      setBusy(false)
      if (err) setDeleteError(err)
      else onClose()
    })
  }

  if (confirmDelete && assistant) {
    return (
      <ConfirmDialog
        title={`Delete ${assistant.name}?`}
        message={`${assistant.name}'s conversation, topics and memory are deleted with it. This can't be undone.`}
        confirmLabel="Delete"
        danger
        busy={busy}
        busyLabel="Deleting…"
        error={deleteError}
        testId="assistant-delete-confirm"
        onConfirm={remove}
        onCancel={() => setConfirmDelete(false)}
      />
    )
  }

  return (
    <Modal onClose={busy ? undefined : onClose} maxWidth={560} data-testid="assistant-dialog">
      <h2>{assistant ? `Edit ${assistant.name}` : 'Add an assistant'}</h2>
      <p className="form-hint">
        Each assistant has its own name, job, voice and conversation. Say its name to talk to it.
      </p>
      <form onSubmit={submit}>
        {!assistant && templates.length > 0 && (
          <div className="form-field">
            <span className="form-label" id="assistant-template-label">
              Start from
            </span>
            <div
              className="assistant-template-grid"
              role="group"
              aria-labelledby="assistant-template-label"
            >
              {templates.map((t) => (
                <button
                  key={t.id}
                  type="button"
                  className="assistant-template"
                  aria-pressed={template === t.id}
                  data-testid={`assistant-template-${t.id}`}
                  onClick={() => applyTemplate(t)}
                >
                  <span className="assistant-template-dot" style={{ background: t.color }} />
                  <span className="assistant-template-title">{t.title}</span>
                  <span className="assistant-template-summary">{t.summary}</span>
                </button>
              ))}
            </div>
          </div>
        )}

        <div className="form-field">
          <label className="form-label" htmlFor="assistant-name">
            Name
          </label>
          <input
            id="assistant-name"
            className="form-input"
            value={name}
            maxLength={MAX_NAME}
            onChange={(e) => setName(e.target.value)}
            onBlur={() => setTouched(true)}
            placeholder="Nova"
            data-testid="assistant-name"
            autoFocus
          />
          <p className="form-hint">
            Pick a name that is easy to say and doesn&rsquo;t sound like the others.
          </p>
          <FieldError message={touched ? nameErr : ''} testId="assistant-name-error" />
        </div>

        <div className="form-field">
          <label className="form-label" htmlFor="assistant-job">
            Job
          </label>
          <textarea
            id="assistant-job"
            className="form-input"
            rows={5}
            value={job}
            onChange={(e) => setJob(e.target.value)}
            placeholder="What this assistant looks after, in a few sentences."
            data-testid="assistant-job"
          />
          <FieldError message={jobErr} testId="assistant-job-error" />
        </div>

        <div className="form-field">
          <label className="form-label" htmlFor="assistant-voice">
            Voice
          </label>
          <div className="assistant-voice-row">
            <select
              id="assistant-voice"
              className="form-input"
              value={voice}
              onChange={(e) => setVoice(e.target.value)}
              data-testid="assistant-voice"
            >
              <option value="">Same as this browser&rsquo;s voice</option>
              {voices.some((v) => isKokoroVoice(v.voiceURI)) && (
                <optgroup label="Natural (server)">
                  {voices
                    .filter((v) => isKokoroVoice(v.voiceURI))
                    .map((v) => (
                      <option key={v.voiceURI} value={v.voiceURI}>
                        {v.name}
                      </option>
                    ))}
                </optgroup>
              )}
              {voices.some((v) => isStudioVoice(v.voiceURI)) && (
                <optgroup label="Speech server">
                  {voices
                    .filter((v) => isStudioVoice(v.voiceURI))
                    .map((v) => (
                      <option key={v.voiceURI} value={v.voiceURI}>
                        {v.name}
                      </option>
                    ))}
                </optgroup>
              )}
              {voices.some((v) => !isServerVoice(v.voiceURI)) && (
                <optgroup label="Browser (this device only)">
                  {voices
                    .filter((v) => !isServerVoice(v.voiceURI))
                    .map((v) => (
                      <option key={v.voiceURI} value={v.voiceURI}>
                        {v.name} ({v.lang})
                      </option>
                    ))}
                </optgroup>
              )}
              {voice && !voices.some((v) => v.voiceURI === voice) && (
                <option value={voice}>{voice} (not on this device)</option>
              )}
            </select>
            <button
              type="button"
              className="btn-secondary"
              onClick={preview}
              data-testid="assistant-voice-preview"
            >
              Preview
            </button>
          </div>
        </div>

        <div className="form-field">
          <span className="form-label" id="assistant-color-label">
            Colour
          </span>
          <div
            className="assistant-color-row"
            role="radiogroup"
            aria-labelledby="assistant-color-label"
          >
            {ASSISTANT_COLORS.map((c) => (
              <button
                key={c.value}
                type="button"
                role="radio"
                aria-checked={color === c.value}
                aria-label={c.label}
                title={c.label}
                className="assistant-color-swatch"
                style={{ background: c.value }}
                data-testid={`assistant-color-${c.label.toLowerCase()}`}
                onClick={() => setColor(c.value)}
              />
            ))}
          </div>
        </div>

        {assistant && !assistant.is_default && (
          <label className="assistant-default-row">
            <input
              type="checkbox"
              checked={makeDefault}
              onChange={(e) => setMakeDefault(e.target.checked)}
              data-testid="assistant-make-default"
            />
            <span>Answer when no name is said (make this the default assistant)</span>
          </label>
        )}

        {error && (
          <p className="form-error" role="alert" data-testid="assistant-dialog-error">
            {error}
          </p>
        )}
        <div className="form-actions">
          {assistant && !assistant.is_default && (
            <button
              type="button"
              className="btn-danger assistant-dialog-delete"
              onClick={() => setConfirmDelete(true)}
              disabled={busy}
              data-testid="assistant-delete"
            >
              Delete
            </button>
          )}
          {touched && reason && (
            <span className="form-actions-reason" data-testid="assistant-dialog-reason">
              {reason}
            </span>
          )}
          <button type="button" className="btn-secondary" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={busy || (touched && !!reason)}
            data-testid="assistant-save"
          >
            {busy ? 'Saving…' : assistant ? 'Save' : 'Add assistant'}
          </button>
        </div>
      </form>
    </Modal>
  )
}
