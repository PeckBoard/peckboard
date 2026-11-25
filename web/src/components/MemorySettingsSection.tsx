import { useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import FieldError from './FieldError'

interface MemorySettings {
  short_term_hours: number
  min_hours: number
  max_hours: number
}

/**
 * Admin-only: how long a session's short-term memory notes last after they
 * were last written (`/api/settings/memory`). Applies to notes written from
 * now on; existing notes keep the time they were given.
 */
export default function MemorySettingsSection() {
  const [settings, setSettings] = useState<MemorySettings | null>(null)
  const [hours, setHours] = useState('')
  const [loadError, setLoadError] = useState<string | null>(null)
  const [saveError, setSaveError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const [saved, setSaved] = useState(false)

  useEffect(() => {
    let cancelled = false
    authedFetch('/api/settings/memory')
      .then((res) => {
        if (!res.ok) throw new Error(`HTTP ${res.status}`)
        return res.json() as Promise<MemorySettings>
      })
      .then((data) => {
        if (cancelled) return
        setSettings(data)
        setHours(String(data.short_term_hours))
      })
      .catch(() => {
        if (!cancelled) setLoadError('Could not load memory settings.')
      })
    return () => {
      cancelled = true
    }
  }, [])

  const value = Number(hours)
  const fieldError =
    settings &&
    (hours.trim() === '' ||
    !Number.isInteger(value) ||
    value < settings.min_hours ||
    value > settings.max_hours
      ? `Enter a whole number of hours from ${settings.min_hours} to ${settings.max_hours}.`
      : undefined)

  const save = async () => {
    if (!settings || fieldError || saving) return
    setSaving(true)
    setSaveError(null)
    setSaved(false)
    try {
      const res = await authedFetch('/api/settings/memory', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ short_term_hours: value }),
      })
      if (!res.ok) {
        const body = (await res.json().catch(() => null)) as { error?: string } | null
        throw new Error(body?.error || 'Could not save memory settings.')
      }
      setSettings({ ...settings, short_term_hours: value })
      setSaved(true)
    } catch (e) {
      setSaveError(e instanceof Error ? e.message : 'Could not save memory settings.')
    } finally {
      setSaving(false)
    }
  }

  return (
    <section
      className="settings-section"
      data-testid="memory-settings-section"
      data-settings-anchor="memory"
    >
      <h3>Agent Memory</h3>
      <p className="form-hint">
        How long the notes an agent keeps about the current work last after they were last written.
        A change applies to notes written from now on.
      </p>
      {loadError && (
        <p className="form-error" role="alert" data-testid="memory-settings-error">
          {loadError}
        </p>
      )}
      {!settings && !loadError && <p className="settings-loading">Loading memory settings...</p>}
      {settings && (
        <>
          <div className="settings-row settings-field-row">
            <label htmlFor="memory-short-term-hours">
              <span className="settings-label">Short-term memory lasts (hours)</span>
            </label>
            <input
              id="memory-short-term-hours"
              className="memory-settings-input"
              type="number"
              min={settings.min_hours}
              max={settings.max_hours}
              step={1}
              value={hours}
              onChange={(e) => {
                setSaved(false)
                setSaveError(null)
                setHours(e.target.value)
              }}
              aria-invalid={!!fieldError}
              data-testid="memory-short-term-hours"
            />
          </div>
          <FieldError message={fieldError || undefined} testId="memory-short-term-hours-error" />
          {saveError && (
            <p className="form-error" role="alert" data-testid="memory-settings-save-error">
              {saveError}
            </p>
          )}
          <div className="settings-row">
            <button
              type="button"
              className="btn-secondary"
              onClick={() => void save()}
              disabled={saving || !!fieldError}
              data-testid="memory-settings-save"
            >
              {saving ? 'Saving…' : 'Save'}
            </button>
            {fieldError && (
              <span className="form-actions-reason">Fix the number of hours to save.</span>
            )}
            {saved && <span className="form-hint">Saved.</span>}
          </div>
        </>
      )}
    </section>
  )
}
