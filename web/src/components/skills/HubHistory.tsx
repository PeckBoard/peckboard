import { useEffect, useState } from 'react'
import List from '../List'
import HubDiff from './HubDiff'
import { getHubDiff, getHubHistory, type HubCommit } from '../../store/skills'

interface Props {
  /** Skill or fork name on Peckboard Public. */
  name: string
  /** Published versions, newest first. */
  versions: string[]
}

const short = (c: string) => c.slice(0, 7)

/**
 * A public skill's history: its commits (newest first, with the version
 * each one published) and the diff between any two versions or commits.
 * Picking a commit shows what it changed.
 */
export default function HubHistory({ name, versions }: Props) {
  const [commits, setCommits] = useState<HubCommit[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [from, setFrom] = useState(versions[1] ?? '')
  const [to, setTo] = useState(versions[0] ?? 'main')
  // Diffs by `from..to`, fetched once each.
  const [diffs, setDiffs] = useState<Record<string, string>>({})
  const [diffError, setDiffError] = useState<string | null>(null)
  const pairKey = from && to && from !== to ? `${from}..${to}` : ''
  const diff = pairKey ? (diffs[pairKey] ?? null) : null

  useEffect(() => {
    let cancelled = false
    getHubHistory(name)
      .then((c) => !cancelled && setCommits(c))
      .catch((e: Error) => !cancelled && setError(e.message))
    return () => {
      cancelled = true
    }
  }, [name])

  useEffect(() => {
    if (!pairKey || diffs[pairKey] !== undefined) return
    let cancelled = false
    getHubDiff(name, from, to)
      .then((d) => {
        if (cancelled) return
        setDiffs((prev) => ({ ...prev, [pairKey]: d }))
        setDiffError(null)
      })
      .catch((e: Error) => !cancelled && setDiffError(e.message))
    return () => {
      cancelled = true
    }
  }, [name, from, to, pairKey, diffs])

  // Every revision the diff can name: the latest text, each version, each
  // commit.
  const revisions: { value: string; label: string }[] = [
    { value: 'main', label: 'Latest text (main)' },
    ...versions.map((v) => ({ value: v, label: `v${v}` })),
    ...(commits ?? []).map((c) => ({
      value: c.commit,
      label: `${short(c.commit)} — ${c.message.split('\n')[0].slice(0, 60)}`,
    })),
  ]
  const select = (id: string, label: string, value: string, set: (v: string) => void) => (
    <>
      <label className="form-label" htmlFor={id}>
        {label}
      </label>
      <select
        id={id}
        className="form-input"
        value={value}
        onChange={(e) => set(e.target.value)}
        data-testid={id}
      >
        <option value="">— pick one —</option>
        {revisions.map((r) => (
          <option key={r.value} value={r.value}>
            {r.label}
          </option>
        ))}
      </select>
    </>
  )

  return (
    <div className="skill-version-viewer" data-testid="skill-hub-history">
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
      <List<HubCommit>
        items={commits ?? []}
        getKey={(c) => c.commit}
        bodyClassName="list-view-rows"
        onActivate={(c) => {
          setFrom(c.parents[0] ?? '')
          setTo(c.commit)
        }}
        renderItem={(c) => (
          <>
            <span className="list-view-name" data-testid={`skill-hub-commit-${short(c.commit)}`}>
              {c.message.split('\n')[0] || short(c.commit)}
            </span>
            <span className="list-view-meta">
              {c.tag && <span className="list-view-tag">v{c.tag.replace(/^v/, '')}</span>}
              <span>{c.author_handle}</span>
              <span>{new Date(c.at).toLocaleString()}</span>
              <span>{short(c.commit)}</span>
            </span>
          </>
        )}
        emptyState={
          <div className="list-view-empty">{commits === null ? 'Loading…' : 'No history yet.'}</div>
        }
      />
      <div className="skill-version-controls">
        {select('skill-history-from', 'Compare', from, setFrom)}
        {select('skill-history-to', 'with', to, setTo)}
      </div>
      {diffError && (
        <p className="form-error" role="alert">
          {diffError}
        </p>
      )}
      {from && to && from === to && <p className="form-hint">Pick two different versions.</p>}
      {diff !== null && <HubDiff diff={diff} testId="skill-history-diff" />}
    </div>
  )
}
