import { useEffect, useState, type FormEvent } from 'react'
import List from '../List'
import Modal from '../Modal'
import EmptyState from '../EmptyState'
import ConfirmDialog from '../ConfirmDialog'
import FieldError from '../FieldError'
import {
  addTeamMemory,
  deleteTeamMemory,
  findingLabel,
  GuardError,
  listTeamMemory,
  SLUG_RE,
  updateTeamMemory,
  useSkillsStore,
  type GuardFinding,
  type TeamMemoryNote,
} from '../../store/skills'
import type { MenuItem } from '../Dropdown'

/** `updated` is free text from the note's frontmatter — show it as a date
 *  when it parses, verbatim otherwise. */
function shortDate(s: string): string {
  const d = new Date(s)
  return isNaN(d.getTime()) ? s : d.toLocaleDateString()
}

/**
 * Team memory: short shared notes kept in a team repository's `memory/`
 * folder, which every teammate's agents can read. Writes go through the
 * server's proprietary-content guard; a rejection lists the offending lines
 * under the text field.
 */
export default function TeamMemoryTab({ onAddSource }: { onAddSource: () => void }) {
  const sources = useSkillsStore((s) => s.sources)
  const teamSources = sources.filter((s) => s.kind === 'team')
  const [chosen, setChosen] = useState('')
  const sourceId = chosen || teamSources[0]?.id || ''
  const [notes, setNotes] = useState<TeamMemoryNote[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [editing, setEditing] = useState<TeamMemoryNote | 'new' | null>(null)
  const [confirmRemove, setConfirmRemove] = useState<TeamMemoryNote | null>(null)
  const [removing, setRemoving] = useState(false)
  const [removeError, setRemoveError] = useState<string | null>(null)

  const [tick, setTick] = useState(0)
  const load = () => setTick((n) => n + 1)
  useEffect(() => {
    if (!sourceId) return
    let cancelled = false
    listTeamMemory(sourceId)
      .then((n) => {
        if (cancelled) return
        setNotes(n)
        setError(null)
      })
      .catch((e: Error) => !cancelled && setError(e.message))
    return () => {
      cancelled = true
    }
  }, [sourceId, tick])

  if (teamSources.length === 0) {
    return (
      <div className="skills-tab" data-testid="skills-memory">
        <EmptyState
          title="No team repository yet"
          body="Team memory lives in a team repository. Add one on the Sources tab to share notes with your team's agents."
          actionLabel="Go to Sources"
          onAction={onAddSource}
          term="skill"
          testId="skills-memory-empty"
        />
      </div>
    )
  }

  const menu = (n: TeamMemoryNote): MenuItem[] => [
    { label: 'Edit', onSelect: () => setEditing(n) },
    { divider: true },
    { label: 'Remove', danger: true, onSelect: () => setConfirmRemove(n) },
  ]

  return (
    <div className="skills-tab" data-testid="skills-memory">
      <div className="registry-controls">
        <select
          className="plugin-setting-select"
          aria-label="Team repository"
          data-testid="skills-memory-source"
          value={sourceId}
          onChange={(e) => {
            setChosen(e.target.value)
            setNotes(null)
          }}
        >
          {teamSources.map((s) => (
            <option key={s.id} value={s.id}>
              {s.name}
            </option>
          ))}
        </select>
        <button
          type="button"
          className="btn-primary btn-sm"
          onClick={() => setEditing('new')}
          data-testid="skill-memory-new"
        >
          + New note
        </button>
      </div>
      {error && (
        <div className="form-error" role="alert">
          <span>{error}</span>{' '}
          <button type="button" className="btn-secondary" onClick={load}>
            Retry
          </button>
        </div>
      )}
      <List<TeamMemoryNote>
        items={notes ?? []}
        getKey={(n) => n.slug}
        bodyClassName="list-view-rows"
        onActivate={(n) => setEditing(n)}
        getMenuItems={menu}
        renderItem={(n) => (
          <>
            <span className="list-view-name" data-testid={`skill-memory-row-${n.slug}`}>
              {n.title || n.slug}
            </span>
            <span className="list-view-meta">
              <span>{n.slug}</span>
              {n.updated && <span>{shortDate(n.updated)}</span>}
              <span className="skill-row-desc">{n.body.split('\n')[0]}</span>
            </span>
          </>
        )}
        emptyState={
          <div className="list-view-empty" data-testid="skill-memory-list-empty">
            {notes === null && !error ? 'Loading…' : 'No notes in this repository yet.'}
          </div>
        }
      />
      {editing && (
        <NoteEditorModal
          sourceId={sourceId}
          note={editing === 'new' ? undefined : editing}
          taken={(notes ?? []).map((n) => n.slug)}
          onClose={() => setEditing(null)}
          onSaved={() => {
            setEditing(null)
            load()
          }}
        />
      )}
      {confirmRemove && (
        <ConfirmDialog
          title="Remove note"
          message={`Remove "${confirmRemove.title || confirmRemove.slug}" from team memory? This commits the removal to the team repository.`}
          confirmLabel="Remove"
          danger
          busy={removing}
          busyLabel="Removing…"
          error={removeError}
          testId="skill-memory-remove-confirm"
          onConfirm={() => {
            setRemoving(true)
            setRemoveError(null)
            deleteTeamMemory(sourceId, confirmRemove.slug)
              .then(() => {
                setConfirmRemove(null)
                load()
              })
              .catch((e: Error) => setRemoveError(e.message))
              .finally(() => setRemoving(false))
          }}
          onCancel={() => {
            if (removing) return
            setConfirmRemove(null)
            setRemoveError(null)
          }}
        />
      )}
    </div>
  )
}

function NoteEditorModal({
  sourceId,
  note,
  taken,
  onClose,
  onSaved,
}: {
  sourceId: string
  note?: TeamMemoryNote
  taken: string[]
  onClose: () => void
  onSaved: () => void
}) {
  const [slug, setSlug] = useState(note?.slug ?? '')
  const [title, setTitle] = useState(note?.title ?? '')
  const [body, setBody] = useState(note?.body ?? '')
  const [touched, setTouched] = useState(false)
  const [findings, setFindings] = useState<GuardFinding[]>([])
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)

  const slugError = note
    ? ''
    : !slug.trim()
      ? 'Give the note a short id'
      : !SLUG_RE.test(slug.trim())
        ? 'Use lowercase letters, digits and dashes (e.g. deploy-steps)'
        : taken.includes(slug.trim())
          ? 'A note with this id already exists'
          : ''
  const titleError = !title.trim() ? 'Give the note a title' : ''
  const bodyError = !body.trim() ? 'Write the note' : ''

  const submit = (e: FormEvent) => {
    e.preventDefault()
    setTouched(true)
    if (slugError || titleError || bodyError) return
    setSaving(true)
    setError(null)
    setFindings([])
    const req = note
      ? updateTeamMemory(sourceId, note.slug, { title: title.trim(), body })
      : addTeamMemory(sourceId, { slug: slug.trim(), title: title.trim(), body })
    req
      .then(onSaved)
      .catch((err: unknown) => {
        if (err instanceof GuardError) setFindings(err.findings)
        else setError(err instanceof Error ? err.message : 'Failed to save note')
      })
      .finally(() => setSaving(false))
  }

  return (
    <Modal onClose={onClose} maxWidth={640} data-testid="skill-memory-modal">
      <h2>{note ? `Edit ${note.title || note.slug}` : 'New team note'}</h2>
      <form onSubmit={submit}>
        <div className="form-field">
          <label className="form-label" htmlFor="skill-memory-title">
            Title
          </label>
          <input
            id="skill-memory-title"
            className="form-input"
            value={title}
            onChange={(e) => {
              setTitle(e.target.value)
              if (!note && (!slug || slug === slugify(title))) setSlug(slugify(e.target.value))
            }}
            data-testid="skill-memory-title"
            autoFocus
          />
          <FieldError message={touched ? titleError : ''} />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="skill-memory-slug">
            Id
          </label>
          <input
            id="skill-memory-slug"
            className="form-input"
            value={slug}
            disabled={!!note}
            onChange={(e) => setSlug(e.target.value)}
            data-testid="skill-memory-slug"
          />
          <FieldError message={touched ? slugError : ''} />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="skill-memory-body">
            Note (markdown)
          </label>
          <textarea
            id="skill-memory-body"
            className="form-input skill-editor-body"
            rows={10}
            value={body}
            onChange={(e) => {
              setBody(e.target.value)
              setFindings([])
            }}
            data-testid="skill-memory-body"
          />
          <FieldError message={touched ? bodyError : ''} />
          {findings.length > 0 && (
            <div className="skill-guard-findings" data-testid="skill-memory-findings">
              <FieldError message="Not saved: this looks like proprietary content. Remove these lines and try again." />
              {findings.map((f, i) => (
                <FieldError key={i} message={findingLabel(f)} testId="skill-memory-finding" />
              ))}
            </div>
          )}
        </div>
        {error && (
          <p className="form-error" role="alert">
            {error}
          </p>
        )}
        <div className="form-actions">
          {touched && (slugError || titleError || bodyError) && (
            <span className="form-actions-reason">Fix the fields marked above</span>
          )}
          <button type="button" className="btn-secondary" onClick={onClose}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={saving}
            data-testid="skill-memory-save"
          >
            {saving ? 'Saving…' : 'Save'}
          </button>
        </div>
      </form>
    </Modal>
  )
}

function slugify(s: string): string {
  return s
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 64)
}
