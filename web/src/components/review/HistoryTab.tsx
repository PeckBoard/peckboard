import { Fragment, useCallback, useEffect, useMemo, useRef, useState } from 'react'

import ConfirmDialog from '../ConfirmDialog'
import {
  formatRelativeTime,
  getVersion,
  listVersions,
  revertToVersion,
  type DocReviewVersionMeta,
} from '../../lib/review'
import { diffLines, hunkHeader, type LineDiff } from '../../lib/lineDiff'
import { describeActionError } from '../../utils/actionError'
import './Review.css'

interface Props {
  reviewId: string
  currentVersion: number
  /** The version the document pane is showing read-only, if any. */
  viewingVersion: number | null
  /** Hand a version to the document pane, or null to go back to current. */
  onView: (v: { version: number; markdown: string } | null) => void
  /** A revert appended a new head version — the screen has to refetch. */
  onReverted: () => void
}

/** 14×14 stroke glyph for who wrote a version. Two shapes, no colour: the
 *  distinction has to survive the contrast floor and colour-blindness. */
function AuthorIcon({ by }: { by: 'user' | 'assistant' }) {
  return (
    <svg
      width="14"
      height="14"
      viewBox="0 0 18 18"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {by === 'user' ? (
        <>
          <circle cx="9" cy="6" r="3" />
          <path d="M3.5 15c.8-2.8 3-4.2 5.5-4.2S13.7 12.2 14.5 15" />
        </>
      ) : (
        <>
          <path d="M9 2.5 10.6 6.6 14.8 8 10.6 9.4 9 13.5 7.4 9.4 3.2 8 7.4 6.6z" />
          <path d="M13.8 12.2l.6 1.5 1.5.6-1.5.6-.6 1.5-.6-1.5-1.5-.6 1.5-.6z" />
        </>
      )}
    </svg>
  )
}

/**
 * Version history for one review: every pass appended a version, nothing was
 * ever overwritten, and this is where that record is read.
 *
 * Two versions are always compared, defaulting to previous ↔ current — the
 * question after a pass is "what did it just change", and answering it should
 * not require picking anything. The diff is computed client-side
 * ([[lineDiff]]) and rendered with the chat's diff classes, so a revision
 * reads the same as a file edit elsewhere in the app.
 */
export default function HistoryTab({
  reviewId,
  currentVersion,
  viewingVersion,
  onView,
  onReverted,
}: Props) {
  const [versions, setVersions] = useState<DocReviewVersionMeta[]>([])
  const [loaded, setLoaded] = useState(false)
  const [error, setError] = useState<string | null>(null)
  /** The pair the user picked, and the head version they picked it at — a
   *  new head re-arms the default (previous ↔ current) without a reset
   *  effect fighting the user's choice. */
  const [picked, setPicked] = useState<{ at: number; versions: number[] } | null>(null)
  const [diff, setDiff] = useState<{ left: number; right: number; result: LineDiff } | null>(null)
  const [diffError, setDiffError] = useState<{
    left: number
    right: number
    message: string
  } | null>(null)
  const [confirmRevert, setConfirmRevert] = useState<number | null>(null)
  const [reverting, setReverting] = useState(false)
  const [revertError, setRevertError] = useState<string | null>(null)

  // A (review, version) body never changes — reverting appends a new version
  // rather than rewriting one — so caching them is safe for the life of the
  // screen. Keyed by review too, in case one mount switches documents.
  const cache = useRef<Map<string, string>>(new Map())

  const load = useCallback(() => {
    listVersions(reviewId)
      .then((vs) => {
        setVersions([...vs].sort((a, b) => b.version - a.version))
        setError(null)
        setLoaded(true)
      })
      .catch((e: unknown) => {
        setError(describeActionError(e, "Couldn't load the version history."))
        setLoaded(true)
      })
  }, [reviewId])

  useEffect(() => {
    load()
  }, [load, currentVersion])
  /** Default comparison: whatever the last pass did. The question after a
   *  pass is "what just changed", and answering it shouldn't need a click. */
  const selection = useMemo(() => {
    if (picked && picked.at === currentVersion) return picked.versions
    return currentVersion > 1 ? [currentVersion - 1, currentVersion] : [currentVersion]
  }, [picked, currentVersion])

  const body = useCallback(
    async (n: number): Promise<string> => {
      const key = `${reviewId}:${n}`
      const hit = cache.current.get(key)
      if (hit !== undefined) return hit
      const v = await getVersion(reviewId, n)
      cache.current.set(key, v.markdown)
      return v.markdown
    },
    [reviewId],
  )

  // Fetch both sides, then diff. Cancelled on re-selection so a slow fetch
  // can't overwrite a newer comparison; the results carry the pair they
  // describe, so a stale one is ignored rather than cleared.
  useEffect(() => {
    if (selection.length !== 2) return
    let cancelled = false
    const [left, right] = selection
    Promise.all([body(left), body(right)])
      .then(([a, b]) => {
        if (!cancelled) setDiff({ left, right, result: diffLines(a, b) })
      })
      .catch((e: unknown) => {
        if (cancelled) return
        setDiffError({
          left,
          right,
          message: describeActionError(e, "Couldn't load those versions."),
        })
      })
    return () => {
      cancelled = true
    }
  }, [selection, body])

  /** Only show results that describe the pair on screen right now. */
  const matches = (r: { left: number; right: number }) =>
    selection.length === 2 && r.left === selection[0] && r.right === selection[1]
  const shownDiff = diff && matches(diff) ? diff : null
  const shownDiffError = diffError && matches(diffError) ? diffError.message : null

  /** Picking a third version drops the oldest — the comparison is always
   *  between the two most recently clicked rows. */
  const toggle = (n: number) => {
    const next = selection.includes(n)
      ? selection.filter((v) => v !== n)
      : [...selection, n].slice(-2).sort((a, b) => a - b)
    setPicked({ at: currentVersion, versions: next })
  }

  const view = (n: number) => {
    if (viewingVersion === n) {
      onView(null)
      return
    }
    body(n)
      .then((markdown) => onView({ version: n, markdown }))
      .catch((e: unknown) => setError(describeActionError(e, "Couldn't open that version.")))
  }

  const revert = () => {
    if (confirmRevert === null) return
    setReverting(true)
    setRevertError(null)
    revertToVersion(reviewId, confirmRevert)
      .then(() => {
        setReverting(false)
        setConfirmRevert(null)
        onView(null)
        onReverted()
      })
      .catch((e: unknown) => {
        setRevertError(describeActionError(e, "Couldn't revert to that version."))
        setReverting(false)
      })
  }

  const stats = useMemo(() => {
    if (!shownDiff) return null
    const { added, removed } = shownDiff.result
    return { added, removed }
  }, [shownDiff])

  return (
    <div className="review-rail__panel review-history" data-testid="review-history">
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}

      {loaded && versions.length > 0 && (
        <section className="review-rail__group">
          <h3 className="review-rail__group-title">
            {shownDiff ? `v${shownDiff.left} → v${shownDiff.right}` : 'Compare'}
            {stats && (
              <span className="review-history__stats">
                {stats.added > 0 && (
                  <span className="review-history__stat-add">+{stats.added}</span>
                )}
                {stats.removed > 0 && (
                  <span className="review-history__stat-del">&minus;{stats.removed}</span>
                )}
              </span>
            )}
          </h3>
          {shownDiffError && (
            <p className="form-error" role="alert">
              {shownDiffError}
            </p>
          )}
          {!shownDiffError && selection.length !== 2 && (
            <p className="review-rail__empty">Pick two versions to compare them.</p>
          )}
          {shownDiff && shownDiff.result.hunks.length === 0 && (
            <p className="review-rail__empty">
              No text changed between v{shownDiff.left} and v{shownDiff.right}.
            </p>
          )}
          {shownDiff && shownDiff.result.hunks.length > 0 && (
            <pre className="diff-body review-history__diff" data-testid="review-diff">
              {shownDiff.result.hunks.map((h, hi) => (
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
              {shownDiff.result.approximate && (
                <div className="diff-line-hunk">
                  … the two versions diverged too far to align line by line
                </div>
              )}
            </pre>
          )}
        </section>
      )}

      <section className="review-rail__group">
        <h3 className="review-rail__group-title">Versions · {versions.length}</h3>
        {!loaded && <div className="loading-spinner" />}
        <ul className="review-rail__list">
          {versions.map((v) => {
            const isPicked = selection.includes(v.version)
            return (
              <li
                key={v.version}
                className={`review-version-row${isPicked ? ' review-version-row--picked' : ''}${
                  viewingVersion === v.version ? ' review-version-row--viewing' : ''
                }`}
                data-testid="review-version-item"
                data-version={v.version}
              >
                <button
                  type="button"
                  className="review-version-row__main"
                  aria-pressed={isPicked}
                  onClick={() => toggle(v.version)}
                >
                  <span className="review-version-row__id">v{v.version}</span>
                  <span className="review-version-row__author" title={v.created_by}>
                    <AuthorIcon by={v.created_by} />
                  </span>
                  <span className="review-version-row__note" title={v.note || undefined}>
                    {v.note || (v.version === 1 ? 'the document as it was' : 'revision')}
                  </span>
                  <span className="review-version-row__time">
                    {formatRelativeTime(v.created_at)}
                  </span>
                </button>
                <div className="review-version-row__actions">
                  <button
                    type="button"
                    className="review-annotation__action"
                    data-testid="review-version-view"
                    onClick={() => view(v.version)}
                  >
                    {viewingVersion === v.version ? 'Close' : 'View'}
                  </button>
                  {v.version !== currentVersion && (
                    <button
                      type="button"
                      className="review-annotation__action"
                      data-testid="review-revert"
                      onClick={() => setConfirmRevert(v.version)}
                    >
                      Revert
                    </button>
                  )}
                </div>
              </li>
            )
          })}
        </ul>
        {loaded && versions.length === 0 && <p className="review-rail__empty">No versions yet.</p>}
      </section>

      {confirmRevert !== null && (
        <ConfirmDialog
          title={`Revert to v${confirmRevert}`}
          message={`Copy v${confirmRevert} back over the document as v${currentVersion + 1}? Nothing is lost — v${currentVersion} stays in the history, and the source file is untouched until you apply.`}
          confirmLabel="Revert"
          busy={reverting}
          error={revertError}
          onConfirm={revert}
          onCancel={() => {
            setConfirmRevert(null)
            setRevertError(null)
          }}
        />
      )}
    </div>
  )
}
