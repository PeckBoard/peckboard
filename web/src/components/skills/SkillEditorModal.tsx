import { useEffect, useState, type FormEvent } from 'react'
import Modal from '../Modal'
import FieldError from '../FieldError'
import SkillMultiPicker from './SkillMultiPicker'
import {
  bumpVersion,
  getSkill,
  SLUG_RE,
  useSkillsStore,
  type Skill,
  type VersionBump,
} from '../../store/skills'

interface Props {
  /** Local skill being edited; omit to create a new one. */
  skill?: Skill
  onClose: () => void
}

const splitTags = (s: string) =>
  s
    .split(',')
    .map((t) => t.trim())
    .filter(Boolean)

/**
 * Create or edit a local skill: name (fixed once created), one-line
 * description, tags, groups and the markdown body. Every save of an existing
 * skill makes a new immutable version — the user picks how far to bump it.
 */
export default function SkillEditorModal({ skill, onClose }: Props) {
  const groups = useSkillsStore((s) => s.groups)
  const createSkill = useSkillsStore((s) => s.createSkill)
  const updateSkill = useSkillsStore((s) => s.updateSkill)
  const editing = !!skill
  const [name, setName] = useState(skill?.name ?? '')
  const [description, setDescription] = useState(skill?.description ?? '')
  const [tags, setTags] = useState((skill?.tags ?? []).join(', '))
  const [skillGroups, setSkillGroups] = useState<string[]>(skill?.groups ?? [])
  const [body, setBody] = useState(skill?.body ?? '')
  const [bodyLoaded, setBodyLoaded] = useState(!skill || skill.body !== undefined)
  const [bump, setBump] = useState<VersionBump>('patch')
  const [touched, setTouched] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)

  // The installed list carries no body; fetch it before editing.
  useEffect(() => {
    if (!skill || skill.body !== undefined) return
    let cancelled = false
    getSkill(skill.id)
      .then((full) => {
        if (cancelled) return
        setBody(full.body ?? '')
        setBodyLoaded(true)
      })
      .catch((e: Error) => !cancelled && setError(e.message))
    return () => {
      cancelled = true
    }
  }, [skill])

  const nameError = editing
    ? ''
    : !name.trim()
      ? 'Give the skill a name'
      : !SLUG_RE.test(name.trim())
        ? 'Use lowercase letters, digits and dashes (e.g. pr-review-checklist)'
        : ''
  const descError = !description.trim() ? 'Say in one line when to use this skill' : ''
  const bodyError = !body.trim() ? 'Write the instructions the agent should follow' : ''
  const invalid = !!(nameError || descError || bodyError) || !bodyLoaded

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setTouched(true)
    if (invalid) return
    setSaving(true)
    setError(null)
    try {
      const fields = {
        description: description.trim(),
        tags: splitTags(tags),
        groups: skillGroups,
        body,
      }
      if (skill) await updateSkill(skill.id, { ...fields, bump })
      else await createSkill({ name: name.trim(), ...fields })
      onClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to save skill')
    } finally {
      setSaving(false)
    }
  }

  const groupOptions = groups
    .filter((g) => g.scope === 'local')
    .map((g) => ({ value: g.slug, label: g.name || g.slug, description: g.description }))

  return (
    <Modal onClose={onClose} maxWidth={720} data-testid="skill-editor-modal">
      <h2>{editing ? `Edit ${skill.name}` : 'New skill'}</h2>
      <form onSubmit={(e) => void submit(e)}>
        <div className="form-field">
          <label className="form-label" htmlFor="skill-editor-name">
            Name
          </label>
          <input
            id="skill-editor-name"
            className="form-input"
            value={name}
            disabled={editing}
            onChange={(e) => setName(e.target.value)}
            placeholder="pr-review-checklist"
            data-testid="skill-editor-name"
            autoFocus={!editing}
          />
          <FieldError message={touched ? nameError : ''} testId="skill-editor-name-error" />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="skill-editor-description">
            Description
          </label>
          <input
            id="skill-editor-description"
            className="form-input"
            value={description}
            onChange={(e) => setDescription(e.target.value)}
            placeholder="When to use it, in one line"
            data-testid="skill-editor-description"
          />
          <FieldError message={touched ? descError : ''} />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="skill-editor-tags">
            Tags <span className="optional">(optional, comma separated)</span>
          </label>
          <input
            id="skill-editor-tags"
            className="form-input"
            value={tags}
            onChange={(e) => setTags(e.target.value)}
            placeholder="review, git"
            data-testid="skill-editor-tags"
          />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="skill-editor-groups">
            Groups <span className="optional">(optional)</span>
          </label>
          <SkillMultiPicker
            id="skill-editor-groups"
            testId="skill-editor-groups"
            ariaLabel="Choose groups"
            placeholder="Search groups…"
            emptyLabel="No groups yet — create one on the Groups tab"
            options={groupOptions}
            selected={skillGroups}
            onChange={setSkillGroups}
          />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="skill-editor-body">
            Instructions (markdown)
          </label>
          <textarea
            id="skill-editor-body"
            className="form-input skill-editor-body"
            rows={14}
            value={body}
            disabled={!bodyLoaded}
            onChange={(e) => setBody(e.target.value)}
            placeholder={bodyLoaded ? '# Checklist\n\n1. …' : 'Loading…'}
            data-testid="skill-editor-body"
          />
          <FieldError message={touched ? bodyError : ''} />
        </div>
        {editing && (
          <div className="form-field">
            <label className="form-label" htmlFor="skill-editor-bump">
              New version
            </label>
            <select
              id="skill-editor-bump"
              className="form-input"
              value={bump}
              onChange={(e) => setBump(e.target.value as VersionBump)}
              data-testid="skill-editor-bump"
            >
              {(['patch', 'minor', 'major'] as VersionBump[]).map((b) => (
                <option key={b} value={b}>
                  {b[0].toUpperCase() + b.slice(1)} — v{bumpVersion(skill.version, b)}
                </option>
              ))}
            </select>
            <p className="form-hint">
              Saving keeps v{skill.version} as it is and adds the new version. Sessions following
              the latest version pick it up on their next start.
            </p>
          </div>
        )}
        {error && (
          <p className="form-error" role="alert" data-testid="skill-editor-error">
            {error}
          </p>
        )}
        <div className="form-actions">
          {touched && invalid && bodyLoaded && (
            <span className="form-actions-reason">Fix the fields marked above</span>
          )}
          <button type="button" className="btn-secondary" onClick={onClose}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={saving || !bodyLoaded}
            data-testid="skill-editor-save"
          >
            {saving ? 'Saving…' : editing ? 'Save new version' : 'Create skill'}
          </button>
        </div>
      </form>
    </Modal>
  )
}
