import { useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'

interface RetentionSettings {
  repeating_session_max_age_days: number
  repeating_session_max_per_task: number
  event_max_age_days: number
  event_max_count_per_session: number
  report_max_age_days: number
  report_max_count: number
  browser_run_max_age_days: number
  tool_image_max_age_days: number
  transcript_max_age_days: number
}

const DEFAULTS: RetentionSettings = {
  repeating_session_max_age_days: 0,
  repeating_session_max_per_task: 0,
  event_max_age_days: 0,
  event_max_count_per_session: 0,
  report_max_age_days: 0,
  report_max_count: 0,
  browser_run_max_age_days: 0,
  tool_image_max_age_days: 0,
  transcript_max_age_days: 0,
}

const FIELDS: Array<{
  key: keyof RetentionSettings
  label: string
  hint: string
}> = [
  {
    key: 'repeating_session_max_age_days',
    label: 'Repeating task runs — max age (days)',
    hint: 'Delete a repeating task run session once it is older than this many days.',
  },
  {
    key: 'repeating_session_max_per_task',
    label: 'Repeating task runs — max kept per task',
    hint: 'Per repeating task, keep only this many of its newest run sessions.',
  },
  {
    key: 'event_max_age_days',
    label: 'Idle session events — max age (days)',
    hint: 'For sessions idle longer than this, delete their events older than the same cutoff.',
  },
  {
    key: 'event_max_count_per_session',
    label: 'Session events — max kept per session',
    hint: 'Keep only this many of the newest events per session. Applies to running sessions too, so a long live session loses its earliest transcript.',
  },
  {
    key: 'report_max_age_days',
    label: 'Reports — max age (days)',
    hint: 'Delete report files older than this many days.',
  },
  {
    key: 'report_max_count',
    label: 'Reports — max kept overall',
    hint: 'Keep only this many of the newest report files.',
  },
  {
    key: 'browser_run_max_age_days',
    label: 'Browser runs — max age (days)',
    hint: 'Delete a recorded browser run (screenshots and page log) once nothing was added to it for this many days.',
  },
  {
    key: 'tool_image_max_age_days',
    label: 'Tool images — max age (days)',
    hint: 'Delete screenshots that tools returned once they are this old. The chat keeps the tool call but no longer shows the image.',
  },
  {
    key: 'transcript_max_age_days',
    label: 'Agent transcripts — max age (days)',
    hint: 'Delete the saved conversations of the Claude, Codex and Grok accounts managed here once they are this old. A session unused for that long keeps its chat history, but its agent starts fresh and no longer remembers it.',
  },
]

interface PruneStats {
  files: number
  bytes: number
}

interface Preview {
  browser_runs: PruneStats
  tool_images: PruneStats
  transcripts: PruneStats
  sessions_reset: number
}

function formatBytes(bytes: number): string {
  if (bytes >= 1e9) return `${(bytes / 1e9).toFixed(1)} GB`
  if (bytes >= 1e6) return `${(bytes / 1e6).toFixed(1)} MB`
  return `${Math.ceil(bytes / 1e3)} KB`
}

function previewLine(name: string, limit: number, stats: PruneStats): string {
  if (limit === 0) return `${name}: kept forever.`
  if (stats.files === 0) return `${name}: nothing is old enough yet.`
  return `${name}: ${stats.files.toLocaleString()} files, ${formatBytes(stats.bytes)}.`
}

/**
 * Admin-only data-retention bounds for repeating-task run sessions, session
 * events, report files, browser runs, tool images and agent transcripts.
 * Enforced hourly by the server's retention sweeper (`service::retention`).
 * Every field is a count/day bound; 0 means keep forever. "Preview" asks the
 * server what the file limits on screen would delete, without deleting.
 */
export default function RetentionSettingsSection() {
  const [settings, setSettings] = useState<RetentionSettings | null>(null)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [saved, setSaved] = useState(false)
  const [preview, setPreview] = useState<Preview | null>(null)
  const [previewing, setPreviewing] = useState(false)

  useEffect(() => {
    let cancelled = false
    authedFetch('/api/settings/retention')
      .then((res) => res.json().then((data) => ({ ok: res.ok, data })))
      .then(({ ok, data }) => {
        if (cancelled) return
        if (!ok) throw new Error('bad status')
        setSettings({ ...DEFAULTS, ...(data as Partial<RetentionSettings>) })
      })
      .catch(() => {
        if (cancelled) return
        setError('Could not load retention settings.')
        setSettings(DEFAULTS)
      })
    return () => {
      cancelled = true
    }
  }, [])

  const setField = (key: keyof RetentionSettings, value: string) => {
    setSaved(false)
    setPreview(null)
    const n = Math.max(0, Math.floor(Number(value) || 0))
    setSettings((prev) => (prev ? { ...prev, [key]: n } : prev))
  }

  const save = async () => {
    if (!settings) return
    setSaving(true)
    setError(null)
    setSaved(false)
    try {
      const res = await authedFetch('/api/settings/retention', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(settings),
      })
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      setSaved(true)
    } catch {
      setError('Could not save retention settings.')
    } finally {
      setSaving(false)
    }
  }
  const runPreview = async () => {
    if (!settings) return
    setPreviewing(true)
    setError(null)
    try {
      const res = await authedFetch('/api/settings/retention/preview', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(settings),
      })
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      setPreview((await res.json()) as Preview)
    } catch {
      setError('Could not work out what would be deleted.')
    } finally {
      setPreviewing(false)
    }
  }

  return (
    <section className="settings-section" data-testid="retention-settings-section">
      <h3>Data retention</h3>
      <p className="form-hint">
        Bounds on repeating-task run sessions, session events, report files, browser runs, tool
        images and agent transcripts, enforced hourly. 0 means keep forever.
      </p>

      {error && (
        <p className="form-error" role="alert" data-testid="retention-settings-error">
          {error}
        </p>
      )}

      {settings === null ? (
        <p className="settings-loading">Loading retention settings...</p>
      ) : (
        <>
          <div className="settings-info-grid">
            {FIELDS.map(({ key, label, hint }) => (
              <div className="settings-row settings-field-row" key={key}>
                <label htmlFor={`retention-${key}`}>
                  <span className="settings-label">{label}</span>
                  <span className="form-hint">{hint}</span>
                </label>
                <input
                  id={`retention-${key}`}
                  type="number"
                  min={0}
                  step={1}
                  value={settings[key]}
                  onChange={(e) => setField(key, e.target.value)}
                  data-testid={`retention-input-${key}`}
                />
              </div>
            ))}
          </div>
          <div className="settings-row">
            <button
              type="button"
              className="btn-secondary"
              onClick={() => void save()}
              disabled={saving}
              data-testid="retention-settings-save"
            >
              {saving ? 'Saving…' : 'Save'}
            </button>
            <button
              type="button"
              className="btn-secondary"
              onClick={() => void runPreview()}
              disabled={previewing}
              data-testid="retention-settings-preview"
            >
              {previewing ? 'Checking…' : 'Preview file cleanup'}
            </button>
            {saved && <span className="form-hint">Saved.</span>}
          </div>
          {preview && (
            <div className="form-hint" data-testid="retention-preview">
              <p>With the limits above, the next cleanup would delete:</p>
              <ul>
                <li>
                  {previewLine(
                    'Browser runs',
                    settings.browser_run_max_age_days,
                    preview.browser_runs,
                  )}
                </li>
                <li>
                  {previewLine(
                    'Tool images',
                    settings.tool_image_max_age_days,
                    preview.tool_images,
                  )}
                </li>
                <li>
                  {previewLine(
                    'Agent transcripts',
                    settings.transcript_max_age_days,
                    preview.transcripts,
                  )}
                  {preview.sessions_reset > 0 &&
                    ` ${preview.sessions_reset.toLocaleString()} unused sessions would start fresh on their next message.`}
                </li>
              </ul>
              <p>Nothing has been deleted. Save to apply the limits.</p>
            </div>
          )}
        </>
      )}
    </section>
  )
}
