import { useCallback, useEffect, useState } from 'react'
import List from '../List'
import Modal from '../Modal'
import EmptyState from '../EmptyState'
import ConfirmDialog from '../ConfirmDialog'
import FieldError from '../FieldError'
import HubDiff from './HubDiff'
import {
  closeMergeRequest,
  commentMergeRequest,
  findingLabel,
  getHubHandle,
  getMergeRequest,
  GuardError,
  HANDLE_RE,
  listMergeRequests,
  mergeMergeRequest,
  setHubHandle,
  useMergeRequestBadge,
  useSkillsStore,
  type GuardFinding,
  type MergeRequest,
  type MrRole,
  type MrStateFilter,
  type VersionBump,
} from '../../store/skills'

const STATE_LABEL: Record<MrStateFilter, string> = {
  open: 'Open',
  merged: 'Merged',
  closed: 'Closed',
  all: 'All',
}

const stateBadge = (s: MergeRequest['state']) =>
  `plugin-badge ${s === 'open' ? 'plugin-badge--pending' : s === 'merged' ? 'plugin-badge--approved' : ''}`

/**
 * Settings → Skills → Merge requests: requests into skills this PeckBoard
 * owns (Incoming) and the ones it sent from its forks (Outgoing), by state.
 * Also where this PeckBoard's handle on Peckboard Public is set.
 */
export default function MergeRequestsTab() {
  const [role, setRole] = useState<MrRole>('incoming')
  const [state, setState] = useState<MrStateFilter>('open')
  const [items, setItems] = useState<MergeRequest[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [openId, setOpenId] = useState<number | null>(null)
  const [tick, setTick] = useState(0)
  const badge = useMergeRequestBadge((s) => s.open)
  const checkBadge = useMergeRequestBadge((s) => s.check)

  useEffect(() => {
    void checkBadge()
  }, [checkBadge])

  useEffect(() => {
    let cancelled = false
    listMergeRequests(role, state)
      .then((l) => {
        if (cancelled) return
        setItems(l)
        setError(null)
      })
      .catch((e: Error) => !cancelled && setError(e.message))
    return () => {
      cancelled = true
    }
  }, [role, state, tick, badge])

  const roles: { id: MrRole; label: string }[] = [
    { id: 'incoming', label: 'Incoming' },
    { id: 'outgoing', label: 'Outgoing' },
  ]

  return (
    <div className="skills-tab" data-testid="skills-merge-requests">
      <HandleField />
      <div className="registry-controls">
        <div className="registry-tabs" role="tablist" aria-label="Direction">
          {roles.map((r) => (
            <button
              key={r.id}
              type="button"
              role="tab"
              aria-selected={role === r.id}
              className={`registry-tab ${role === r.id ? 'registry-tab--active' : ''}`}
              onClick={() => {
                if (r.id !== role) setItems(null)
                setRole(r.id)
              }}
              data-testid={`skill-mr-role-${r.id}`}
            >
              {r.label}
              {r.id === 'incoming' && badge > 0 && (
                <span className="plugin-badge plugin-badge--pending"> {badge}</span>
              )}
            </button>
          ))}
        </div>
        <select
          className="plugin-setting-select"
          aria-label="State"
          value={state}
          onChange={(e) => {
            setItems(null)
            setState(e.target.value as MrStateFilter)
          }}
          data-testid="skill-mr-state"
        >
          {(Object.keys(STATE_LABEL) as MrStateFilter[]).map((s) => (
            <option key={s} value={s}>
              {STATE_LABEL[s]}
            </option>
          ))}
        </select>
      </div>
      {error && (
        <div className="form-error" role="alert">
          <span>{error}</span>{' '}
          <button type="button" className="btn-secondary" onClick={() => setTick((n) => n + 1)}>
            Retry
          </button>
        </div>
      )}
      <List<MergeRequest>
        items={items ?? []}
        getKey={(m) => String(m.id)}
        bodyClassName="list-view-rows"
        onActivate={(m) => setOpenId(m.id)}
        renderItem={(m) => (
          <>
            <span className="list-view-name" data-testid={`skill-mr-row-${m.id}`}>
              {m.title}
            </span>
            <span className="list-view-meta skill-mr-meta">
              <span className={stateBadge(m.state)}>{STATE_LABEL[m.state]}</span>
              <span>
                {m.fork} → {m.target}
              </span>
              <span>by {m.author_handle}</span>
              {m.state === 'open' && !m.mergeable && (
                <span className="list-view-tag">Needs an update from the fork</span>
              )}
            </span>
          </>
        )}
        emptyState={
          items === null && !error ? (
            <div className="list-view-empty">Loading…</div>
          ) : (
            <EmptyState
              title={role === 'incoming' ? 'No merge requests for you' : 'No merge requests sent'}
              body={
                role === 'incoming'
                  ? 'When someone forks a skill you published and asks you to merge their changes, it shows up here.'
                  : 'Fork a public skill under Browse, edit it, then use "Ask the author to merge" to send your changes.'
              }
              term="skill"
              testId="skill-mr-empty"
            />
          )
        }
      />
      {openId !== null && (
        <MergeRequestModal
          id={openId}
          role={role}
          onClose={() => setOpenId(null)}
          onChanged={() => {
            setTick((n) => n + 1)
            void checkBadge()
          }}
        />
      )}
    </div>
  )
}

/** This PeckBoard's handle on Peckboard Public: forks are named
 *  `<skill>@<handle>`. Freeform, validated like the hub does. */
function HandleField() {
  const [current, setCurrent] = useState<string | null>(null)
  const [value, setValue] = useState('')
  const [loadError, setLoadError] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [saved, setSaved] = useState(false)
  const [publicKey, setPublicKey] = useState('')

  useEffect(() => {
    getHubHandle()
      .then((h) => {
        setCurrent(h.handle)
        setValue(h.handle)
        setPublicKey(h.public_key ?? '')
      })
      .catch((e: Error) => setLoadError(e.message))
  }, [])

  // `k-…` is reserved for the handle Peckboard Public gives each install.
  const invalid = !value
    ? ''
    : !HANDLE_RE.test(value)
      ? '2 to 32 lowercase letters, digits or dashes.'
      : value.startsWith('k-') && value !== current
        ? 'Handles starting with "k-" are reserved. Pick another.'
        : ''
  const unchanged = value === current
  const save = () => {
    setBusy(true)
    setError(null)
    setSaved(false)
    setHubHandle(value)
      .then((h) => {
        setCurrent(h.handle)
        setValue(h.handle)
        setSaved(true)
      })
      .catch((e: Error) => setError(e.message))
      .finally(() => setBusy(false))
  }

  if (loadError) return null
  return (
    <div className="form-field" data-testid="skill-hub-handle">
      <label className="form-label" htmlFor="skill-hub-handle-input">
        Your handle on Peckboard Public
      </label>
      <div className="registry-controls">
        <input
          id="skill-hub-handle-input"
          className="form-input"
          value={value}
          disabled={busy || current === null}
          onChange={(e) => {
            setValue(e.target.value.trim())
            setSaved(false)
          }}
          data-testid="skill-hub-handle-input"
        />
        <button
          type="button"
          className="btn-secondary btn-sm"
          onClick={save}
          disabled={busy || !value || !!invalid || unchanged}
          data-testid="skill-hub-handle-save"
        >
          {busy ? 'Saving…' : 'Save'}
        </button>
      </div>
      <FieldError message={invalid || error || undefined} testId="skill-hub-handle-error" />
      <p className="form-hint">
        Your forks are named <code>skill@{value || 'handle'}</code>.{saved ? ' Saved.' : ''}
      </p>
      {publicKey && (
        <p className="form-hint" data-testid="skill-hub-key">
          This PeckBoard&apos;s publisher key: <code>{publicKey}</code>. It owns the skills you
          publish; share it only if you ask for a skill to be moved to you.
        </p>
      )}
    </div>
  )
}

function MergeRequestModal({
  id,
  role,
  onClose,
  onChanged,
}: {
  id: number
  role: MrRole
  onClose: () => void
  onChanged: () => void
}) {
  const fetchSkills = useSkillsStore((s) => s.fetchSkills)
  const [mr, setMr] = useState<MergeRequest | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [comment, setComment] = useState('')
  const [publish, setPublish] = useState(true)
  const [bump, setBump] = useState<VersionBump>('minor')
  const [busy, setBusy] = useState(false)
  const [actionError, setActionError] = useState<string | null>(null)
  const [findings, setFindings] = useState<GuardFinding[]>([])
  const [confirmClose, setConfirmClose] = useState(false)
  const [notice, setNotice] = useState<string | null>(null)

  const load = useCallback(() => {
    getMergeRequest(id)
      .then(setMr)
      .catch((e: Error) => setError(e.message))
  }, [id])
  useEffect(load, [load])

  const fail = (err: unknown) => {
    if (err instanceof GuardError) setFindings(err.findings)
    else setActionError(err instanceof Error ? err.message : 'Something went wrong')
  }

  const doMerge = () => {
    setBusy(true)
    setActionError(null)
    setFindings([])
    mergeMergeRequest(id, publish ? { publish, bump } : { publish })
      .then((res) => {
        setMr((prev) => ({ ...(prev ?? res.merge_request), ...res.merge_request }))
        setNotice(
          res.merge_request.merged_version
            ? `Merged and published as v${res.merge_request.merged_version}.`
            : 'Merged.',
        )
        void fetchSkills()
        onChanged()
      })
      .catch(fail)
      .finally(() => setBusy(false))
  }

  const doComment = () => {
    setBusy(true)
    setActionError(null)
    commentMergeRequest(id, comment.trim())
      .then((c) => {
        setComment('')
        setMr((prev) => (prev ? { ...prev, comments: [...(prev.comments ?? []), c] } : prev))
      })
      .catch(fail)
      .finally(() => setBusy(false))
  }

  const open = mr?.state === 'open'
  const canMerge = role === 'incoming' && open
  return (
    <Modal onClose={busy ? () => {} : onClose} maxWidth={820} data-testid="skill-mr-modal">
      {!mr ? (
        error ? (
          <p className="form-error" role="alert">
            {error}
          </p>
        ) : (
          <p className="settings-loading">Loading…</p>
        )
      ) : (
        <>
          <h2>
            {mr.title}{' '}
            <span className={stateBadge(mr.state)} data-testid="skill-mr-state-badge">
              {STATE_LABEL[mr.state]}
            </span>
          </h2>
          <p className="form-hint">
            <code>{mr.fork}</code> → <code>{mr.target}</code>, by {mr.author_handle}
            {mr.merged_version ? ` · published as v${mr.merged_version}` : ''}
          </p>
          {mr.description && <p data-testid="skill-mr-description-text">{mr.description}</p>}
          {open && !mr.mergeable && (
            <p className="form-error" role="status" data-testid="skill-mr-not-mergeable">
              Needs an update from the fork: it no longer merges cleanly into {mr.target}. The
              fork&apos;s author can sync it from the original, fix what clashes and publish again.
            </p>
          )}
          <HubDiff diff={mr.diff ?? ''} testId="skill-mr-diff" />
          <div className="skill-mr-comments" data-testid="skill-mr-comments">
            <h3>Comments</h3>
            {(mr.comments ?? []).length === 0 && <p className="form-hint">No comments yet.</p>}
            {(mr.comments ?? []).map((c) => (
              <div key={c.id} className="form-field" data-testid="skill-mr-comment">
                <p className="form-hint">
                  {c.author_handle} · {new Date(c.at).toLocaleString()}
                </p>
                <p>{c.body}</p>
              </div>
            ))}
            <div className="form-field">
              <label className="form-label" htmlFor="skill-mr-comment-input">
                Add a comment
              </label>
              <textarea
                id="skill-mr-comment-input"
                className="form-input"
                rows={3}
                value={comment}
                disabled={busy}
                onChange={(e) => setComment(e.target.value)}
                data-testid="skill-mr-comment-input"
              />
              <div className="skill-mr-comment-actions">
                <button
                  type="button"
                  className="btn-secondary btn-sm"
                  disabled={busy || !comment.trim()}
                  onClick={doComment}
                  data-testid="skill-mr-comment-submit"
                >
                  Comment
                </button>
              </div>
            </div>
          </div>
          {canMerge && (
            <div className="skill-mr-publish">
              <label className="form-checkbox-label">
                <input
                  type="checkbox"
                  checked={publish}
                  disabled={busy}
                  onChange={(e) => setPublish(e.target.checked)}
                  data-testid="skill-mr-publish"
                />{' '}
                Publish a new version
              </label>
              {publish && (
                <select
                  className="form-input"
                  aria-label="Version bump"
                  value={bump}
                  disabled={busy}
                  onChange={(e) => setBump(e.target.value as VersionBump)}
                  data-testid="skill-mr-bump"
                >
                  <option value="patch">Patch (small fix)</option>
                  <option value="minor">Minor (new or changed steps)</option>
                  <option value="major">Major (works differently)</option>
                </select>
              )}
            </div>
          )}
          {findings.length > 0 && (
            <div className="skill-guard-findings">
              <FieldError message="Not published: the merged text looks like it has proprietary content." />
              {findings.map((f, i) => (
                <FieldError key={i} message={findingLabel(f)} />
              ))}
            </div>
          )}
          {actionError && (
            <p className="form-error" role="alert" data-testid="skill-mr-action-error">
              {actionError}
            </p>
          )}
          {notice && (
            <p className="form-hint" role="status" data-testid="skill-mr-notice">
              {notice}
            </p>
          )}
        </>
      )}
      <div className="form-actions">
        {canMerge && mr && !mr.mergeable && (
          <span className="form-actions-reason">Can&apos;t merge until the fork is updated</span>
        )}
        {open && (
          <button
            type="button"
            className="btn-secondary"
            disabled={busy}
            onClick={() => setConfirmClose(true)}
            data-testid="skill-mr-close-request"
          >
            Close request
          </button>
        )}
        {canMerge && (
          <button
            type="button"
            className="btn-primary"
            disabled={busy || !mr?.mergeable}
            onClick={doMerge}
            data-testid="skill-mr-merge"
          >
            {busy ? 'Working…' : 'Merge'}
          </button>
        )}
        <button type="button" className="btn-secondary" onClick={onClose} disabled={busy}>
          Done
        </button>
      </div>
      {confirmClose && (
        <ConfirmDialog
          title="Close merge request"
          message={`Close "${mr?.title}" without merging it? The fork keeps its changes.`}
          confirmLabel="Close request"
          danger
          busy={busy}
          error={actionError}
          testId="skill-mr-close-confirm"
          onConfirm={() => {
            setBusy(true)
            setActionError(null)
            closeMergeRequest(id)
              .then((res) => {
                setMr((prev) => ({ ...(prev ?? res), ...res }))
                setConfirmClose(false)
                onChanged()
              })
              .catch(fail)
              .finally(() => setBusy(false))
          }}
          onCancel={() => !busy && setConfirmClose(false)}
        />
      )}
    </Modal>
  )
}
