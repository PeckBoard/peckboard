import { useCallback, useEffect, useState } from 'react'
import List from './List'
import ListViewHeader from './ListViewHeader'
import DiffBlock from './DiffBlock'
import { listRepos, repoDiff, type RepoDiffResult, type RepoEntry } from '../lib/review'
import { useFoldersStore } from '../store/folders'

interface Props {
  /** The folder whose tree was scanned (`/folders/<id>/repos`). */
  folderId: string
  /** Back to the Folders page. */
  onBack: () => void
}

/** Human status chip text for a diffed file. */
const STATUS_LABEL: Record<string, string> = {
  added: 'added',
  deleted: 'deleted',
  renamed: 'renamed',
  modified: 'modified',
  untracked: 'untracked',
}

/**
 * `/folders/<id>/repos` — every git repo found in the folder's tree (the
 * same recursive scan `GET /api/repos` runs), each opening its own diff
 * viewer: the repo's working-tree changes vs HEAD, one collapsible unified
 * diff per file. Discovery and the diff both stay folder-jailed on the
 * server; this page only ever names what the scan reported.
 */
export default function RepoBrowserPage({ folderId, onBack }: Props) {
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)
  const folderName = folders.find((f) => f.id === folderId)?.name

  const [repos, setRepos] = useState<RepoEntry[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  // Repo whose diff viewer is open (folder-relative path; '' = folder root
  // — a real value, so this must stay `null` for "list view", not falsy).
  const [openRepo, setOpenRepo] = useState<string | null>(null)
  const [diff, setDiff] = useState<RepoDiffResult | null>(null)
  const [diffError, setDiffError] = useState<string | null>(null)
  const [diffLoading, setDiffLoading] = useState(false)

  useEffect(() => {
    fetchFolders()
  }, [fetchFolders])

  // No synchronous setState here: the effect below calls this on mount, and
  // resetting state inline would cascade renders (react-hooks lint). A rescan
  // keeps the previous list on screen until the fresh one lands.
  const loadRepos = useCallback(() => {
    listRepos(folderId)
      .then((r) => {
        setRepos(r)
        setError(null)
      })
      .catch((e) => setError(e instanceof Error ? e.message : "Couldn't list the repos"))
  }, [folderId])

  useEffect(() => {
    loadRepos()
  }, [loadRepos])

  const loadDiff = useCallback(
    (repoPath: string) => {
      setDiffLoading(true)
      setDiffError(null)
      setDiff(null)
      repoDiff(folderId, repoPath)
        .then(setDiff)
        .catch((e) => setDiffError(e instanceof Error ? e.message : "Couldn't diff that repo"))
        .finally(() => setDiffLoading(false))
    },
    [folderId],
  )

  const openDiff = (repoPath: string) => {
    setOpenRepo(repoPath)
    loadDiff(repoPath)
  }

  // ---- Diff viewer for one repo ----
  if (openRepo !== null) {
    const entry = repos?.find((r) => r.path === openRepo)
    const title = diff?.name ?? entry?.name ?? openRepo ?? folderName ?? 'Repo'
    return (
      <div className="list-view" data-testid="repo-diff-view">
        <ListViewHeader
          title={
            <span className="repo-diff-title">
              <button
                type="button"
                className="repo-back-btn"
                onClick={() => {
                  setOpenRepo(null)
                  setDiff(null)
                  setDiffError(null)
                }}
                aria-label="Back to repo list"
                data-testid="repo-diff-back"
              >
                ‹
              </button>
              {title}
              {diff && (
                <span className="repo-branch-chip" title="Checked-out branch">
                  {diff.branch}
                </span>
              )}
            </span>
          }
          actionLabel="Refresh"
          onAction={() => loadDiff(openRepo)}
          actionTestId="repo-diff-refresh"
        />
        <div className="list-view-body repo-diff-body">
          {diffLoading && (
            <div className="chat-loading">
              <div className="loading-spinner" />
            </div>
          )}
          {diffError && (
            <p className="form-error" role="alert">
              {diffError}
            </p>
          )}
          {diff && diff.files.length === 0 && (
            <div className="list-view-empty" data-testid="repo-diff-clean">
              <p>Working tree clean — no changes vs HEAD.</p>
            </div>
          )}
          {diff && diff.files.length > 0 && (
            <>
              <p className="repo-diff-summary" data-testid="repo-diff-summary">
                {diff.files.length} changed {diff.files.length === 1 ? 'file' : 'files'}
                {' · '}
                <span className="diff-added">
                  +{diff.files.reduce((n, f) => n + f.added, 0)}
                </span>{' '}
                <span className="diff-removed">
                  &minus;{diff.files.reduce((n, f) => n + f.removed, 0)}
                </span>
                {diff.truncated && ' · file list truncated'}
              </p>
              {diff.files.map((f) => (
                <div key={f.path} className="repo-diff-file">
                  <span className={`repo-file-status repo-file-status-${f.status}`}>
                    {STATUS_LABEL[f.status] ?? f.status}
                  </span>
                  <DiffBlock
                    diff={{
                      path: f.path,
                      diff: f.diff,
                      added: f.added,
                      removed: f.removed,
                      truncated: f.truncated,
                      created: f.status === 'added' || f.status === 'untracked',
                    }}
                  />
                </div>
              ))}
            </>
          )}
        </div>
      </div>
    )
  }

  // ---- Repo list ----
  return (
    <div className="list-view" data-testid="repo-list-view">
      <ListViewHeader
        title={folderName ? `Repos — ${folderName}` : 'Repos'}
        extras={
          <button
            type="button"
            className="btn-secondary"
            onClick={onBack}
            data-testid="repo-list-back"
          >
            ‹ Folders
          </button>
        }
        actionLabel="Rescan"
        onAction={loadRepos}
        actionTestId="repo-list-rescan"
      />
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
      {repos === null && !error && (
        <div className="chat-loading">
          <div className="loading-spinner" />
        </div>
      )}
      {repos !== null && (
        <List
          items={repos}
          getKey={(r) => r.path || '.'}
          onActivate={(r) => openDiff(r.path)}
          emptyState={
            <div className="list-view-empty" data-testid="repo-list-empty">
              <p>No git repos found in this folder or its subfolders.</p>
            </div>
          }
          renderItem={(r) => (
            <>
              <span className="list-view-name">{r.name}</span>
              <span className="list-view-meta">
                <span className="repo-branch-chip">{r.worktrees[0]?.branch ?? '?'}</span>
                <span className="list-view-tag repo-path-tag">{r.path || '(folder root)'}</span>
                {r.worktrees.length > 1 && (
                  <span className="list-view-time">
                    {r.worktrees.length - 1} linked{' '}
                    {r.worktrees.length - 1 === 1 ? 'worktree' : 'worktrees'}
                  </span>
                )}
              </span>
            </>
          )}
        />
      )}
    </div>
  )
}
