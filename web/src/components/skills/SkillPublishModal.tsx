import { useState } from 'react'
import Modal from '../Modal'
import FieldError from '../FieldError'
import {
  findingLabel,
  GuardError,
  publishSkill,
  useSkillsStore,
  type GuardFinding,
  type PublishResult,
  type Skill,
  type SkillSource,
} from '../../store/skills'

interface Props {
  skill: Skill
  onClose: () => void
}

/**
 * Share a local skill's newest version. A team repository gets it committed
 * and pushed; the public catalog gets it written to this server's outbox,
 * with the steps to open the pull request. The proprietary-content guard runs
 * first either way, and its findings are listed here.
 */
export default function SkillPublishModal({ skill, onClose }: Props) {
  const sources = useSkillsStore((s) => s.sources)
  const fetchSources = useSkillsStore((s) => s.fetchSources)
  // Team repositories first, then the public catalog.
  const targets: SkillSource[] = [
    ...sources.filter((s) => s.kind === 'team'),
    ...sources.filter((s) => s.kind === 'public'),
  ]
  const [chosen, setChosen] = useState('')
  const targetId = chosen || targets[0]?.id || ''
  const target = targets.find((s) => s.id === targetId)
  const [busy, setBusy] = useState(false)
  const [findings, setFindings] = useState<GuardFinding[]>([])
  const [error, setError] = useState<string | null>(null)
  const [result, setResult] = useState<{ source: SkillSource; res: PublishResult } | null>(null)

  const submit = () => {
    if (!target) return
    setBusy(true)
    setError(null)
    setFindings([])
    publishSkill(skill.id, target.id)
      .then((res) => {
        setResult({ source: target, res })
        if (target.kind === 'team') void fetchSources()
      })
      .catch((err: unknown) => {
        if (err instanceof GuardError) setFindings(err.findings)
        else setError(err instanceof Error ? err.message : 'Failed to publish')
      })
      .finally(() => setBusy(false))
  }

  return (
    <Modal onClose={busy ? () => {} : onClose} maxWidth={640} data-testid="skill-publish-modal">
      <h2>
        Publish {skill.name} <span className="wasm-plugin-version">v{skill.version}</span>
      </h2>
      {result ? (
        <PublishDone source={result.source} res={result.res} />
      ) : (
        <>
          <p className="form-hint">
            Publishes the newest version. Teammates subscribed to a team repository get it on their
            next sync; the public catalog takes a pull request. The text is checked for secrets and
            internal details first.
          </p>
          <div className="form-field">
            <label className="form-label" htmlFor="skill-publish-target">
              Publish to
            </label>
            <select
              id="skill-publish-target"
              className="form-input"
              value={targetId}
              disabled={busy}
              onChange={(e) => {
                setChosen(e.target.value)
                setFindings([])
                setError(null)
              }}
              data-testid="skill-publish-target"
            >
              {targets.map((s) => (
                <option key={s.id} value={s.id}>
                  {s.kind === 'public' ? `${s.name} (public catalog)` : `${s.name} (team)`}
                </option>
              ))}
            </select>
            {targets.length === 0 && (
              <FieldError message="No repository to publish to. Add a team repository on the Sources tab." />
            )}
            {findings.length > 0 && (
              <div className="skill-guard-findings" data-testid="skill-publish-findings">
                <FieldError message="Not published: this looks like proprietary content. Edit the skill to remove these and try again." />
                {findings.map((f, i) => (
                  <FieldError key={i} message={findingLabel(f)} testId="skill-publish-finding" />
                ))}
              </div>
            )}
          </div>
          {error && (
            <p className="form-error" role="alert" data-testid="skill-publish-error">
              {error}
            </p>
          )}
        </>
      )}
      <div className="form-actions">
        {result ? (
          <button
            type="button"
            className="btn-primary"
            onClick={onClose}
            data-testid="skill-publish-close"
          >
            Close
          </button>
        ) : (
          <>
            <button type="button" className="btn-secondary" onClick={onClose} disabled={busy}>
              Cancel
            </button>
            <button
              type="button"
              className="btn-primary"
              onClick={submit}
              disabled={busy || !target}
              data-testid="skill-publish-submit"
            >
              {busy ? 'Publishing…' : 'Publish'}
            </button>
          </>
        )}
      </div>
    </Modal>
  )
}

function PublishDone({ source, res }: { source: SkillSource; res: PublishResult }) {
  if (source.kind === 'team') {
    return (
      <div className="skill-publish-result" data-testid="skill-publish-result" role="status">
        <p>
          {res.committed === false
            ? `${source.name} already has this version — nothing new to push.`
            : `Published to ${source.name}.`}
        </p>
        <p className="form-hint">
          In the repository at <code>{res.path}</code>.
        </p>
      </div>
    )
  }
  return (
    <div className="skill-publish-result" data-testid="skill-publish-result" role="status">
      <p>Ready for {source.name}. One step left: open a pull request.</p>
      {res.instructions && <p className="form-hint">{res.instructions}</p>}
      <p className="form-hint">
        Path in the repository: <code>{res.path}</code>
      </p>
      {res.local_path && (
        <p className="form-hint">
          File on this Peckboard server:{' '}
          <code data-testid="skill-publish-local-path">{res.local_path}</code>
        </p>
      )}
      {res.pr_url && (
        <p>
          <a href={res.pr_url} target="_blank" rel="noreferrer">
            Open the pull request
          </a>
        </p>
      )}
    </div>
  )
}
