import { useEffect, useState } from 'react'
import Modal from '../Modal'
import SkillVersionViewer from './SkillVersionViewer'
import {
  compareVersions,
  inUseVersion,
  SCOPE_LABEL,
  searchCatalog,
  useSkillsStore,
  type Skill,
} from '../../store/skills'

interface Props {
  skill: Skill
  onClose: () => void
  onEdit?: () => void
  /** Local skills only: opens the publish dialog. */
  onPublish?: () => void
}

const byVersionDesc = (a: string, b: string) => compareVersions(b, a)

/**
 * One installed skill: what it is, which version is in use, any version's
 * text and a diff between two versions. A subscription's pinned version is
 * changed here ("Latest" follows each new release).
 */
export default function SkillDetailModal({ skill, onClose, onEdit, onPublish }: Props) {
  const setPinnedVersion = useSkillsStore((s) => s.setPinnedVersion)
  const [pinBusy, setPinBusy] = useState(false)
  const [pinError, setPinError] = useState<string | null>(null)
  // Versions stored here (readable / diffable) vs everything the source
  // publishes (pinnable — pinning fetches the version).
  const versions = skill.versions.length > 0 ? skill.versions : [skill.version]
  const [published, setPublished] = useState<string[]>([])
  useEffect(() => {
    if (!skill.subscribed || !skill.source_id) return
    let cancelled = false
    searchCatalog(skill.name, skill.source_id)
      .then(({ items }) => {
        const hit = items.find((i) => i.name === skill.name)
        if (!cancelled && hit) setPublished(hit.available_versions)
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [skill.subscribed, skill.source_id, skill.name])
  const pinnable = [...new Set([...published, ...versions])].sort(byVersionDesc)

  const changePin = (value: string) => {
    setPinBusy(true)
    setPinError(null)
    setPinnedVersion(skill.id, value || null)
      .catch((e: Error) => setPinError(e.message))
      .finally(() => setPinBusy(false))
  }

  return (
    <Modal onClose={onClose} maxWidth={760} data-testid="skill-detail-modal">
      <h2>
        {skill.name} <span className="wasm-plugin-version">v{inUseVersion(skill)}</span>
      </h2>
      <div className="skill-detail-meta">
        <span className="list-view-tag">{SCOPE_LABEL[skill.scope]}</span>
        {skill.tags.map((t) => (
          <span key={t} className="list-view-tag">
            #{t}
          </span>
        ))}
        {skill.groups.map((g) => (
          <span key={g} className="list-view-tag">
            group: {g}
          </span>
        ))}
      </div>
      {skill.description && <p className="form-hint">{skill.description}</p>}

      {skill.subscribed && (
        <div className="form-field">
          <label className="form-label" htmlFor="skill-version-select">
            Version in use
          </label>
          <select
            id="skill-version-select"
            className="form-input"
            value={skill.pinned_version ?? ''}
            disabled={pinBusy}
            onChange={(e) => changePin(e.target.value)}
            data-testid="skill-version-select"
          >
            <option value="">Latest (v{pinnable[0] ?? skill.version})</option>
            {pinnable.map((v) => (
              <option key={v} value={v}>
                v{v}
              </option>
            ))}
          </select>
          {pinError && (
            <p className="form-error" role="alert">
              {pinError}
            </p>
          )}
        </div>
      )}

      <SkillVersionViewer skillId={skill.id} versions={versions} initial={inUseVersion(skill)} />

      <div className="form-actions">
        {onEdit && (
          <button
            type="button"
            className="btn-secondary"
            onClick={onEdit}
            data-testid="skill-detail-edit"
          >
            Edit
          </button>
        )}
        {onPublish && (
          <button
            type="button"
            className="btn-secondary"
            onClick={onPublish}
            data-testid="skill-detail-publish"
          >
            Publish…
          </button>
        )}
        <button type="button" className="btn-primary" onClick={onClose}>
          Close
        </button>
      </div>
    </Modal>
  )
}
