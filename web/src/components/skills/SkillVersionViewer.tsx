import { Fragment, useEffect, useMemo, useRef, useState } from 'react'
import { diffLines, hunkHeader } from '../../lib/lineDiff'
import { compareVersions, getSkillVersion } from '../../store/skills'

interface Props {
  skillId: string
  versions: string[]
  /** Version selected when the viewer opens. */
  initial: string
}

/**
 * Read any version of a skill, or line-diff two of them. Bodies are fetched
 * on demand (`GET /api/skills/{id}/versions/{v}`) and cached per viewer.
 */
export default function SkillVersionViewer({ skillId, versions, initial }: Props) {
  const [view, setView] = useState(initial)
  const [against, setAgainst] = useState('')
  const [bodies, setBodies] = useState<Record<string, string>>({})
  const [error, setError] = useState<string | null>(null)

  // Each version is requested once; a late answer after unmount is a no-op.
  const requested = useRef(new Set<string>())
  useEffect(() => {
    for (const v of [view, against]) {
      if (!v || requested.current.has(v)) continue
      requested.current.add(v)
      getSkillVersion(skillId, v)
        .then((b) => setBodies((prev) => ({ ...prev, [v]: b })))
        .catch((e: Error) => {
          requested.current.delete(v)
          setError(e.message)
        })
    }
  }, [skillId, view, against])

  // Older version is always the "before" side, whichever way round the user
  // picked them, so + lines are what the newer version added.
  const [older, newer] = compareVersions(against, view) <= 0 ? [against, view] : [view, against]
  const diff = useMemo(() => {
    if (!against || bodies[older] === undefined || bodies[newer] === undefined) return null
    return diffLines(bodies[older], bodies[newer])
  }, [against, older, newer, bodies])

  return (
    <div className="skill-version-viewer">
      <div className="skill-version-controls">
        <label className="form-label" htmlFor="skill-view-version">
          View version
        </label>
        <select
          id="skill-view-version"
          className="form-input"
          value={view}
          onChange={(e) => setView(e.target.value)}
          data-testid="skill-view-version"
        >
          {versions.map((v) => (
            <option key={v} value={v}>
              v{v}
            </option>
          ))}
        </select>
        <label className="form-label" htmlFor="skill-diff-against">
          Compare with
        </label>
        <select
          id="skill-diff-against"
          className="form-input"
          value={against}
          onChange={(e) => setAgainst(e.target.value)}
          data-testid="skill-diff-against"
          disabled={versions.length < 2}
        >
          <option value="">— no comparison —</option>
          {versions
            .filter((v) => v !== view)
            .map((v) => (
              <option key={v} value={v}>
                v{v}
              </option>
            ))}
        </select>
      </div>
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
      {!against &&
        (bodies[view] === undefined ? (
          <p className="settings-loading">Loading…</p>
        ) : (
          <pre className="diff-body skill-body-preview" data-testid="skill-version-body">
            {bodies[view]}
          </pre>
        ))}
      {against && !diff && <p className="settings-loading">Loading…</p>}
      {diff && diff.hunks.length === 0 && (
        <p className="form-hint">
          No text changed between v{older} and v{newer}.
        </p>
      )}
      {diff && diff.hunks.length > 0 && (
        <p className="form-hint">
          Changes from v{older} to v{newer}: +{diff.added} / &minus;{diff.removed} lines
        </p>
      )}
      {diff && diff.hunks.length > 0 && (
        <pre className="diff-body skill-body-preview" data-testid="skill-version-diff">
          {diff.hunks.map((h, hi) => (
            <Fragment key={hi}>
              <div className="diff-line-hunk">{hunkHeader(h)}</div>
              {h.lines.map((l, li) => (
                <div
                  key={li}
                  className={
                    l.op === 'add'
                      ? 'diff-line-add'
                      : l.op === 'del'
                        ? 'diff-line-del'
                        : 'diff-line'
                  }
                >
                  {(l.op === 'add' ? '+' : l.op === 'del' ? '-' : ' ') + (l.text || '')}
                </div>
              ))}
            </Fragment>
          ))}
        </pre>
      )}
    </div>
  )
}
