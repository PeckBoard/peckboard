import { useState, useEffect } from 'react'
import { authedFetch } from '../store/auth'
import { useGitStore, type CommitDetail, type DiscoveredRepo } from '../store/git'

export default function GitView() {
  const discoveredRepos = useGitStore((s) => s.discoveredRepos)
  const loadingRepos = useGitStore((s) => s.loadingRepos)
  const fileStatuses = useGitStore((s) => s.fileStatuses)
  const commits = useGitStore((s) => s.commits)
  const fetchedPath = useGitStore((s) => s.fetchedPath)
  const loadingStatus = useGitStore((s) => s.loadingStatus)
  const statusError = useGitStore((s) => s.statusError)
  const fetchRepos = useGitStore((s) => s.fetchRepos)
  const fetchStatus = useGitStore((s) => s.fetchStatus)

  const [repoPath, setRepoPath] = useState('')
  const [activeCommit, setActiveCommit] = useState<CommitDetail | null>(null)
  const [loadingCommit, setLoadingCommit] = useState(false)
  const [commitError, setCommitError] = useState('')

  const loading = loadingStatus
  const fetched = fetchedPath !== null
  const error = statusError || commitError

  useEffect(() => {
    fetchRepos()
  }, [fetchRepos])

  const selectRepo = (repo: DiscoveredRepo) => {
    setRepoPath(repo.path)
    setActiveCommit(null)
    fetchStatus(repo.path)
  }

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault()
    if (repoPath.trim()) {
      setActiveCommit(null)
      fetchStatus(repoPath.trim())
    }
  }

  const viewCommit = async (hash: string) => {
    setLoadingCommit(true)
    setActiveCommit(null)
    setCommitError('')
    try {
      const res = await authedFetch(
        `/api/git/commits/${encodeURIComponent(hash)}?path=${encodeURIComponent(repoPath)}`,
      )
      if (!res.ok) throw new Error('Failed to load commit')
      const data: CommitDetail = await res.json()
      setActiveCommit(data)
    } catch {
      setCommitError('Failed to load commit details')
    } finally {
      setLoadingCommit(false)
    }
  }

  const statusColor = (status: string): string => {
    switch (status.toLowerCase()) {
      case 'modified':
      case 'm':
        return 'var(--warning)'
      case 'added':
      case 'a':
      case '?':
      case '??':
        return 'var(--success)'
      case 'deleted':
      case 'd':
        return 'var(--danger)'
      default:
        return 'var(--text2)'
    }
  }

  return (
    <div className="settings-page" style={{ maxWidth: 800 }}>
      <h2>Git</h2>

      <section className="settings-section">
        <h3>Repository</h3>
        {loadingRepos ? (
          <div className="chat-loading">
            <div className="loading-spinner" />
          </div>
        ) : discoveredRepos.length > 0 ? (
          <div className="folder-list" style={{ marginBottom: 12 }}>
            {discoveredRepos.map((repo) => (
              <button
                key={`${repo.folder_id}-${repo.path}`}
                className="folder-row"
                style={{
                  cursor: 'pointer',
                  width: '100%',
                  border:
                    repoPath === repo.path ? '1px solid var(--accent)' : '1px solid transparent',
                  background: repoPath === repo.path ? 'var(--surface2)' : 'transparent',
                  borderRadius: 'var(--radius)',
                  textAlign: 'left',
                  font: 'inherit',
                  color: 'inherit',
                  padding: '8px 12px',
                }}
                onClick={() => selectRepo(repo)}
              >
                <div className="folder-info">
                  <strong style={{ fontSize: 'var(--text-sm)' }}>{repo.name}</strong>
                  <span className="folder-path">{repo.path}</span>
                </div>
                <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text3)' }}>
                  {repo.folder_name}
                </span>
              </button>
            ))}
          </div>
        ) : null}
        <form onSubmit={handleSubmit} style={{ display: 'flex', gap: 8 }}>
          <input
            className="form-input"
            style={{ flex: 1 }}
            placeholder="Repository path (e.g. /Users/me/projects/myrepo)"
            value={repoPath}
            onChange={(e) => setRepoPath(e.target.value)}
          />
          <button
            className="btn-primary"
            type="submit"
            disabled={loading || !repoPath.trim()}
            style={{ whiteSpace: 'nowrap' }}
          >
            {loading ? 'Loading...' : 'Fetch'}
          </button>
        </form>
        {error && (
          <p className="form-error" style={{ marginTop: 8 }}>
            {error}
          </p>
        )}
      </section>

      {fetched && (
        <>
          <section className="settings-section">
            <h3>Working Tree Status</h3>
            {fileStatuses.length === 0 ? (
              <p style={{ color: 'var(--text3)', fontSize: 'var(--text-sm)' }}>
                Working tree is clean.
              </p>
            ) : (
              <div className="folder-list">
                {fileStatuses.map((f) => (
                  <div key={f.path} className="folder-row">
                    <span
                      style={{
                        fontFamily: 'var(--font-mono)',
                        fontSize: 'var(--text-xs)',
                        fontWeight: 700,
                        color: statusColor(f.status),
                        width: 24,
                        flexShrink: 0,
                      }}
                    >
                      {f.status}
                    </span>
                    <span
                      style={{
                        fontFamily: 'var(--font-mono)',
                        fontSize: 'var(--text-sm)',
                        color: 'var(--text)',
                      }}
                    >
                      {f.path}
                    </span>
                  </div>
                ))}
              </div>
            )}
          </section>

          <section className="settings-section">
            <h3>Recent Commits</h3>
            {commits.length === 0 ? (
              <p style={{ color: 'var(--text3)', fontSize: 'var(--text-sm)' }}>No commits found.</p>
            ) : (
              <div className="folder-list">
                {commits.map((c) => (
                  <div key={c.hash} className="folder-row" style={{ cursor: 'pointer' }}>
                    <button
                      style={{
                        flex: 1,
                        border: 'none',
                        background: 'transparent',
                        textAlign: 'left',
                        cursor: 'pointer',
                        padding: 0,
                        font: 'inherit',
                        color: 'inherit',
                      }}
                      onClick={() => viewCommit(c.hash)}
                    >
                      <div className="folder-info">
                        <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                          <span
                            style={{
                              fontFamily: 'var(--font-mono)',
                              fontSize: 'var(--text-xs)',
                              color: 'var(--accent)',
                              fontWeight: 600,
                            }}
                          >
                            {c.short_hash}
                          </span>
                          <strong style={{ fontSize: 'var(--text-sm)' }}>{c.message}</strong>
                        </div>
                        <span className="folder-path">
                          {c.author} &middot; {c.date}
                        </span>
                      </div>
                    </button>
                  </div>
                ))}
              </div>
            )}
          </section>

          {loadingCommit && (
            <section className="settings-section">
              <div className="chat-loading">
                <div className="loading-spinner" />
              </div>
            </section>
          )}

          {activeCommit && (
            <section className="settings-section">
              <h3>Commit Detail</h3>
              <div style={{ marginBottom: 12 }}>
                <div className="settings-row">
                  <span className="settings-label">Hash</span>
                  <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-xs)' }}>
                    {activeCommit.hash}
                  </span>
                </div>
                <div className="settings-row">
                  <span className="settings-label">Author</span>
                  <span>{activeCommit.author}</span>
                </div>
                <div className="settings-row">
                  <span className="settings-label">Date</span>
                  <span>{activeCommit.date}</span>
                </div>
                <div className="settings-row">
                  <span className="settings-label">Message</span>
                  <span>{activeCommit.message}</span>
                </div>
              </div>
              {activeCommit.diff && (
                <pre className="tool-pre" style={{ maxHeight: 500 }}>
                  {activeCommit.diff}
                </pre>
              )}
            </section>
          )}
        </>
      )}
    </div>
  )
}
