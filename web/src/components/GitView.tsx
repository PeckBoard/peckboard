import { useState, useCallback } from 'react'
import { authedFetch } from '../store/auth'

interface FileStatus {
  path: string
  status: string
}

interface Commit {
  hash: string
  short_hash: string
  author: string
  date: string
  message: string
}

interface CommitDetail {
  hash: string
  author: string
  date: string
  message: string
  diff: string
}

export default function GitView() {
  const [repoPath, setRepoPath] = useState('')
  const [fileStatuses, setFileStatuses] = useState<FileStatus[]>([])
  const [commits, setCommits] = useState<Commit[]>([])
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState('')
  const [activeCommit, setActiveCommit] = useState<CommitDetail | null>(null)
  const [loadingCommit, setLoadingCommit] = useState(false)
  const [fetched, setFetched] = useState(false)

  const fetchStatus = useCallback(async (path: string) => {
    setLoading(true)
    setError('')
    setFetched(false)
    setActiveCommit(null)
    try {
      const [statusRes, commitsRes] = await Promise.all([
        authedFetch(`/api/git/status?path=${encodeURIComponent(path)}`),
        authedFetch(`/api/git/commits?path=${encodeURIComponent(path)}&limit=20`),
      ])

      if (!statusRes.ok) {
        const data = await statusRes.json().catch(() => ({ error: 'Failed to fetch git status' }))
        throw new Error(data.error || 'Failed to fetch git status')
      }
      if (!commitsRes.ok) {
        const data = await commitsRes.json().catch(() => ({ error: 'Failed to fetch commits' }))
        throw new Error(data.error || 'Failed to fetch commits')
      }

      const statusData: FileStatus[] = await statusRes.json()
      const commitsData: Commit[] = await commitsRes.json()
      setFileStatuses(statusData)
      setCommits(commitsData)
      setFetched(true)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch git data')
    } finally {
      setLoading(false)
    }
  }, [])

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault()
    if (repoPath.trim()) {
      fetchStatus(repoPath.trim())
    }
  }

  const viewCommit = async (hash: string) => {
    setLoadingCommit(true)
    setActiveCommit(null)
    try {
      const res = await authedFetch(
        `/api/git/commits/${encodeURIComponent(hash)}?path=${encodeURIComponent(repoPath)}`
      )
      if (!res.ok) throw new Error('Failed to load commit')
      const data: CommitDetail = await res.json()
      setActiveCommit(data)
    } catch {
      setError('Failed to load commit details')
    } finally {
      setLoadingCommit(false)
    }
  }

  const statusColor = (status: string): string => {
    switch (status.toLowerCase()) {
      case 'modified': case 'm': return 'var(--warning)'
      case 'added': case 'a': case '?': case '??': return 'var(--success)'
      case 'deleted': case 'd': return 'var(--danger)'
      default: return 'var(--text2)'
    }
  }

  return (
    <div className="settings-page" style={{ maxWidth: 800 }}>
      <h2>Git</h2>

      <section className="settings-section">
        <h3>Repository</h3>
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
        {error && <p className="form-error" style={{ marginTop: 8 }}>{error}</p>}
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
                    <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-sm)', color: 'var(--text)' }}>
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
                        flex: 1, border: 'none', background: 'transparent', textAlign: 'left',
                        cursor: 'pointer', padding: 0, font: 'inherit', color: 'inherit',
                      }}
                      onClick={() => viewCommit(c.hash)}
                    >
                      <div className="folder-info">
                        <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                          <span style={{
                            fontFamily: 'var(--font-mono)', fontSize: 'var(--text-xs)',
                            color: 'var(--accent)', fontWeight: 600,
                          }}>
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
              <div className="chat-loading"><div className="loading-spinner" /></div>
            </section>
          )}

          {activeCommit && (
            <section className="settings-section">
              <h3>Commit Detail</h3>
              <div style={{ marginBottom: 12 }}>
                <div className="settings-row">
                  <span className="settings-label">Hash</span>
                  <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-xs)' }}>{activeCommit.hash}</span>
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
