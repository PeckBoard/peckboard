import { useEffect, useMemo, useState } from 'react'
import Modal from '../Modal'
import HelpTip from '../HelpTip'
import SkillMultiPicker from './SkillMultiPicker'
import { useSkillOptions, splitSkillSelection } from './skillOptions'
import { getSessionSkills, putSessionSkills, type SessionSkills } from '../../store/skills'

interface Props {
  sessionId: string
  onClose: () => void
}

/**
 * Choose which skills (and whole groups of skills) a session's agent can
 * load. Opened from the "Skills" entry of every session menu; saves through
 * `PUT /api/sessions/{id}/skills`.
 */
export default function SessionSkillsModal({ sessionId, onClose }: Props) {
  const options = useSkillOptions()
  const [current, setCurrent] = useState<SessionSkills | null>(null)
  const [selected, setSelected] = useState<string[]>([])
  const [loadError, setLoadError] = useState<string | null>(null)
  const [saveError, setSaveError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    let cancelled = false
    getSessionSkills(sessionId)
      .then((s) => {
        if (cancelled) return
        setCurrent(s)
        setSelected([...s.groups.map((g) => `group:${g}`), ...s.skills.map((k) => `skill:${k}`)])
      })
      .catch((e: Error) => !cancelled && setLoadError(e.message))
    return () => {
      cancelled = true
    }
  }, [sessionId])

  const dirty = useMemo(() => {
    if (!current) return false
    const was = [
      ...current.groups.map((g) => `group:${g}`),
      ...current.skills.map((k) => `skill:${k}`),
    ]
    return was.length !== selected.length || was.some((v) => !selected.includes(v))
  }, [current, selected])

  const save = async () => {
    setSaving(true)
    setSaveError(null)
    try {
      const next = await putSessionSkills(sessionId, splitSkillSelection(selected))
      setCurrent(next)
      onClose()
    } catch (e) {
      setSaveError(e instanceof Error ? e.message : 'Failed to save skills')
    } finally {
      setSaving(false)
    }
  }

  return (
    <Modal onClose={onClose} maxWidth={560} data-testid="session-skills-modal">
      <h2>
        Session skills <HelpTip term="skill" />
      </h2>
      <p className="form-hint">
        The agent sees the names and descriptions of the chosen skills and loads a skill&rsquo;s
        full text when it needs it. A group brings in every skill it holds.
      </p>
      {loadError && (
        <p className="form-error" role="alert" data-testid="session-skills-error">
          {loadError}
        </p>
      )}
      {!loadError && current === null && <p className="settings-loading">Loading…</p>}
      {current && (
        <>
          <div className="form-field">
            <label className="form-label" htmlFor="session-skills-picker">
              Skills and groups
            </label>
            <SkillMultiPicker
              id="session-skills-picker"
              testId="session-skills-picker"
              ariaLabel="Choose skills and groups"
              placeholder="Search skills and groups…"
              emptyLabel="No skills installed yet — add some in Settings → Skills"
              options={options}
              selected={selected}
              onChange={setSelected}
            />
          </div>
          {current.resolved.length > 0 && (
            <div className="form-field" data-testid="session-skills-resolved">
              <span className="form-label">Currently loaded</span>
              <ul className="skill-resolved-list">
                {current.resolved.map((s) => (
                  <li key={s.id}>
                    <strong>{s.name}</strong>{' '}
                    <span className="wasm-plugin-version">v{s.active_version ?? s.version}</span>
                    {s.description && <span className="form-hint"> — {s.description}</span>}
                  </li>
                ))}
              </ul>
            </div>
          )}
        </>
      )}
      {saveError && (
        <p className="form-error" role="alert">
          {saveError}
        </p>
      )}
      <div className="form-actions">
        {current && !dirty && <span className="form-actions-reason">No changes to save</span>}
        <button type="button" className="btn-secondary" onClick={onClose}>
          Cancel
        </button>
        <button
          type="button"
          className="btn-primary"
          disabled={!current || !dirty || saving}
          onClick={() => void save()}
          data-testid="session-skills-save"
        >
          {saving ? 'Saving…' : 'Save'}
        </button>
      </div>
    </Modal>
  )
}
