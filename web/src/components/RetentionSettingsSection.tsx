import { useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'

interface RetentionSettings {
  repeating_session_max_age_days: number
  repeating_session_max_per_task: number
  event_max_age_days: number
  event_max_count_per_session: number
  report_max_age_days: number
  report_max_count: number
}

const DEFAULTS: RetentionSettings = {
  repeating_session_max_age_days: 0,
  repeating_session_max_per_task: 0,
  event_max_age_days: 0,
  event_max_count_per_session: 0,
  report_max_age_days: 0,
  report_max_count: 0,
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
    hint: 'Keep only this many of the newest events per session.',
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
]

/**
 * Admin-only data-retention bounds for repeating-task run sessions, session
 * events, and report files. Enforced hourly by the server's retention
 * sweeper (`service::retention`). Every field is a count/day bound; 0 means
 * keep forever.
 */
export default function RetentionSettingsSection() {
  const [settings, setSettings] = useState<RetentionSettings | null>(null)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [saved, setSaved] = useState(false)

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

  return (
    <section className="settings-section" data-testid="retention-settings-section">
      <h3>Data retention</h3>
      <p className="form-hint">
        Bounds on repeating-task run sessions, session events, and report files, enforced hourly. 0
        means keep forever.
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
              <div className="settings-row" key={key}>
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
            {saved && <span className="form-hint">Saved.</span>}
          </div>
        </>
      )}
    </section>
  )
}
