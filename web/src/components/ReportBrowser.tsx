import { useEffect, useState, useCallback } from 'react'
import { authedFetch } from '../store/auth'

interface ReportEntry {
  folder: string
  file: string
  title: string
  date: string
}

interface GroupedReports {
  [folder: string]: ReportEntry[]
}

export default function ReportBrowser() {
  const [reports, setReports] = useState<ReportEntry[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState('')
  const [expandedFolders, setExpandedFolders] = useState<Set<string>>(new Set())
  const [activeReport, setActiveReport] = useState<{ folder: string; file: string } | null>(null)
  const [reportContent, setReportContent] = useState('')
  const [loadingContent, setLoadingContent] = useState(false)

  const fetchReports = useCallback(async () => {
    setLoading(true)
    setError('')
    try {
      const res = await authedFetch('/api/reports')
      if (!res.ok) {
        const data = await res.json().catch(() => ({ error: 'Failed to fetch reports' }))
        throw new Error(data.error || 'Failed to fetch reports')
      }
      const data: ReportEntry[] = await res.json()
      setReports(data)
      // Auto-expand all folders
      const folders = new Set(data.map((r) => r.folder))
      setExpandedFolders(folders)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch reports')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => { fetchReports() }, [fetchReports])

  const toggleFolder = (folder: string) => {
    setExpandedFolders((prev) => {
      const next = new Set(prev)
      if (next.has(folder)) {
        next.delete(folder)
      } else {
        next.add(folder)
      }
      return next
    })
  }

  const viewReport = async (folder: string, file: string) => {
    setActiveReport({ folder, file })
    setLoadingContent(true)
    setReportContent('')
    try {
      const res = await authedFetch(`/api/reports/${encodeURIComponent(folder)}/${encodeURIComponent(file)}`)
      if (!res.ok) throw new Error('Failed to load report')
      const text = await res.text()
      setReportContent(text)
    } catch {
      setReportContent('<p>Failed to load report content.</p>')
    } finally {
      setLoadingContent(false)
    }
  }

  const grouped: GroupedReports = {}
  for (const r of reports) {
    if (!grouped[r.folder]) grouped[r.folder] = []
    grouped[r.folder].push(r)
  }

  if (activeReport) {
    return (
      <div className="settings-page" style={{ maxWidth: 'none' }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 12, marginBottom: 24 }}>
          <button className="btn-secondary" onClick={() => setActiveReport(null)}>
            &larr; Back
          </button>
          <h2 style={{ margin: 0 }}>Report</h2>
          <a
            href={`/api/reports/${encodeURIComponent(activeReport.folder)}/${encodeURIComponent(activeReport.file)}/download`}
            className="btn-secondary"
            style={{ marginLeft: 'auto', textDecoration: 'none' }}
            download
          >
            Download
          </a>
        </div>
        {loadingContent ? (
          <div className="chat-loading"><div className="loading-spinner" /></div>
        ) : (
          <div dangerouslySetInnerHTML={{ __html: reportContent }} />
        )}
      </div>
    )
  }

  return (
    <div className="settings-page">
      <h2>Reports</h2>

      {loading && <div className="chat-loading"><div className="loading-spinner" /></div>}
      {error && <p className="form-error">{error}</p>}

      {!loading && reports.length === 0 && !error && (
        <p style={{ color: 'var(--text3)', fontSize: 'var(--text-sm)' }}>No reports found.</p>
      )}

      {Object.keys(grouped).map((folder) => (
        <section key={folder} className="settings-section">
          <h3
            style={{ cursor: 'pointer', userSelect: 'none' }}
            onClick={() => toggleFolder(folder)}
          >
            <span style={{ display: 'inline-block', width: 16, fontSize: 10 }}>
              {expandedFolders.has(folder) ? '\u25BC' : '\u25B6'}
            </span>
            {folder}
          </h3>
          {expandedFolders.has(folder) && (
            <div className="folder-list">
              {grouped[folder].map((r) => (
                <div key={`${r.folder}/${r.file}`} className="folder-row" style={{ cursor: 'pointer' }}>
                  <button
                    style={{
                      flex: 1, border: 'none', background: 'transparent', textAlign: 'left',
                      cursor: 'pointer', padding: 0, font: 'inherit', color: 'inherit',
                    }}
                    onClick={() => viewReport(r.folder, r.file)}
                  >
                    <div className="folder-info">
                      <strong>{r.title || r.file}</strong>
                      <span className="folder-path">{r.date}</span>
                    </div>
                  </button>
                  <a
                    href={`/api/reports/${encodeURIComponent(r.folder)}/${encodeURIComponent(r.file)}/download`}
                    className="btn-secondary"
                    style={{ textDecoration: 'none', fontSize: 'var(--text-xs)', padding: '4px 10px' }}
                    download
                    onClick={(e) => e.stopPropagation()}
                  >
                    Download
                  </a>
                </div>
              ))}
            </div>
          )}
        </section>
      ))}
    </div>
  )
}
