import { useState, type FormEvent } from 'react'
import List from '../List'
import Modal from '../Modal'
import EmptyState from '../EmptyState'
import ConfirmDialog from '../ConfirmDialog'
import FieldError from '../FieldError'
import SkillMultiPicker from './SkillMultiPicker'
import { SCOPE_LABEL, SLUG_RE, useSkillsStore, type SkillGroup } from '../../store/skills'
import type { MenuItem } from '../Dropdown'

/**
 * Skill groups: named bundles a session can be given in one pick. Local
 * groups are edited here; groups that come from a source are read-only.
 */
export default function SkillGroupsTab() {
  const groups = useSkillsStore((s) => s.groups)
  const loaded = useSkillsStore((s) => s.groupsLoaded)
  const deleteGroup = useSkillsStore((s) => s.deleteGroup)
  const [editing, setEditing] = useState<SkillGroup | 'new' | null>(null)
  const [confirmDelete, setConfirmDelete] = useState<SkillGroup | null>(null)
  const [deleting, setDeleting] = useState(false)
  const [deleteError, setDeleteError] = useState<string | null>(null)

  const menu = (g: SkillGroup): MenuItem[] =>
    g.scope === 'local'
      ? [
          { label: 'Edit', onSelect: () => setEditing(g) },
          { divider: true },
          { label: 'Delete', danger: true, onSelect: () => setConfirmDelete(g) },
        ]
      : []

  return (
    <div className="skills-tab" data-testid="skills-groups">
      <div className="settings-section-head">
        <p className="form-hint">
          Give a session a whole group and it gets every skill in it, including ones added later.
        </p>
        <button
          type="button"
          className="btn-primary btn-sm"
          onClick={() => setEditing('new')}
          data-testid="skill-group-new"
        >
          + New group
        </button>
      </div>
      <List<SkillGroup>
        items={groups}
        getKey={(g) => g.id}
        bodyClassName="list-view-rows"
        onActivate={(g) => g.scope === 'local' && setEditing(g)}
        getMenuItems={menu}
        renderItem={(g) => (
          <>
            <span className="list-view-name" data-testid={`skill-group-row-${g.slug}`}>
              {g.name || g.slug}
            </span>
            <span className="list-view-meta">
              <span className="list-view-tag">{SCOPE_LABEL[g.scope]}</span>
              <span>
                {g.skills.length} {g.skills.length === 1 ? 'skill' : 'skills'}
              </span>
              <span className="skill-row-desc">{g.description}</span>
            </span>
          </>
        )}
        emptyState={
          loaded ? (
            <EmptyState
              title="No groups yet"
              body="Bundle skills that go together — say, everything for reviewing a pull request — so a session can take them in one pick."
              actionLabel="+ New group"
              onAction={() => setEditing('new')}
              term="skill"
              testId="skill-groups-empty"
            />
          ) : (
            <div className="list-view-empty">Loading…</div>
          )
        }
      />
      {editing && (
        <GroupEditorModal
          group={editing === 'new' ? undefined : editing}
          onClose={() => setEditing(null)}
        />
      )}
      {confirmDelete && (
        <ConfirmDialog
          title="Delete group"
          message={`Delete the group "${confirmDelete.name || confirmDelete.slug}"? Its skills stay installed; sessions given this group lose them on their next start.`}
          confirmLabel="Delete"
          danger
          busy={deleting}
          busyLabel="Deleting…"
          error={deleteError}
          testId="skill-group-delete-confirm"
          onConfirm={() => {
            setDeleting(true)
            setDeleteError(null)
            deleteGroup(confirmDelete.id)
              .then(() => setConfirmDelete(null))
              .catch((e: Error) => setDeleteError(e.message))
              .finally(() => setDeleting(false))
          }}
          onCancel={() => {
            if (deleting) return
            setConfirmDelete(null)
            setDeleteError(null)
          }}
        />
      )}
    </div>
  )
}

function GroupEditorModal({ group, onClose }: { group?: SkillGroup; onClose: () => void }) {
  const skills = useSkillsStore((s) => s.skills)
  const groups = useSkillsStore((s) => s.groups)
  const saveGroup = useSkillsStore((s) => s.saveGroup)
  const [slug, setSlug] = useState(group?.slug ?? '')
  const [name, setName] = useState(group?.name ?? '')
  const [description, setDescription] = useState(group?.description ?? '')
  const [members, setMembers] = useState<string[]>(group?.skills ?? [])
  const [touched, setTouched] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)

  const slugError = !slug.trim()
    ? 'Give the group a short id'
    : !SLUG_RE.test(slug.trim())
      ? 'Use lowercase letters, digits and dashes (e.g. pr-review)'
      : groups.some((g) => g.slug === slug.trim() && g.id !== group?.id && g.scope === 'local')
        ? 'Another group already uses this id'
        : ''
  const nameError = !name.trim() ? 'Give the group a name' : ''

  const submit = (e: FormEvent) => {
    e.preventDefault()
    setTouched(true)
    if (slugError || nameError) return
    setSaving(true)
    setError(null)
    saveGroup(group?.id ?? null, {
      slug: slug.trim(),
      name: name.trim(),
      description: description.trim(),
      skills: members,
    })
      .then(onClose)
      .catch((err: Error) => setError(err.message))
      .finally(() => setSaving(false))
  }

  // Membership is by skill name (the contract's group file lists names).
  const options = [...new Set(skills.map((s) => s.name))].sort().map((n) => ({
    value: n,
    label: n,
    description: skills.find((s) => s.name === n)?.description,
  }))

  return (
    <Modal onClose={onClose} maxWidth={560} data-testid="skill-group-modal">
      <h2>{group ? `Edit ${group.name || group.slug}` : 'New group'}</h2>
      <form onSubmit={submit}>
        <div className="form-field">
          <label className="form-label" htmlFor="skill-group-name">
            Name
          </label>
          <input
            id="skill-group-name"
            className="form-input"
            value={name}
            onChange={(e) => {
              setName(e.target.value)
              if (!group && (!slug || slug === slugify(name))) setSlug(slugify(e.target.value))
            }}
            placeholder="PR review"
            data-testid="skill-group-name"
            autoFocus
          />
          <FieldError message={touched ? nameError : ''} />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="skill-group-slug">
            Id
          </label>
          <input
            id="skill-group-slug"
            className="form-input"
            value={slug}
            onChange={(e) => setSlug(e.target.value)}
            placeholder="pr-review"
            data-testid="skill-group-slug"
          />
          <FieldError message={touched ? slugError : ''} testId="skill-group-slug-error" />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="skill-group-description">
            Description <span className="optional">(optional)</span>
          </label>
          <input
            id="skill-group-description"
            className="form-input"
            value={description}
            onChange={(e) => setDescription(e.target.value)}
            data-testid="skill-group-description"
          />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="skill-group-skills">
            Skills
          </label>
          <SkillMultiPicker
            id="skill-group-skills"
            testId="skill-group-skills"
            ariaLabel="Choose skills"
            placeholder="Search skills…"
            emptyLabel="No skills installed yet"
            options={options}
            selected={members}
            onChange={setMembers}
          />
        </div>
        {error && (
          <p className="form-error" role="alert">
            {error}
          </p>
        )}
        <div className="form-actions">
          {touched && (slugError || nameError) && (
            <span className="form-actions-reason">Fix the fields marked above</span>
          )}
          <button type="button" className="btn-secondary" onClick={onClose}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={saving}
            data-testid="skill-group-save"
          >
            {saving ? 'Saving…' : group ? 'Save' : 'Create group'}
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
