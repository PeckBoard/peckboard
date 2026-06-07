import { useEffect, useState, useCallback } from 'react'
import { authedFetch } from '../store/auth'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'

interface ReportEntry {
  folder: string
  file: string
  title: string
  date: string
  sessionId?: string
  projectName?: string
}

interface GroupedReports {
  [folder: string]: ReportEntry[]
}

export default function ReportBrowser() {
  const [reports, setReports] = useState<ReportEntry[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState('')
  const [expandedFolders, setExpandedFolders] = useState<Set<string>>(new Set())
  const [activeReport, setActiveReport] = useState<{
    folder: string
    file: string
    title?: string
    sessionId?: string
    projectName?: string
  } | null>(null)
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
      const data = await res.json()
      const list: ReportEntry[] = Array.isArray(data) ? data : (data.reports ?? [])
      setReports(list)
      const folders = new Set(list.map((r: ReportEntry) => r.folder))
      setExpandedFolders(folders)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch reports')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    fetchReports()
  }, [fetchReports])

  const toggleFolder = (folder: string) => {
    setExpandedFolders((prev) => {
      const next = new Set(prev)
      if (next.has(folder)) next.delete(folder)
      else next.add(folder)
      return next
    })
  }

  const viewReport = async (report: ReportEntry) => {
    setActiveReport({
      folder: report.folder,
      file: report.file,
      title: report.title,
      sessionId: report.sessionId,
      projectName: report.projectName,
    })
    setLoadingContent(true)
    setReportContent('')
    try {
      const res = await authedFetch(
        `/api/reports/${encodeURIComponent(report.folder)}/${encodeURIComponent(report.file)}`,
      )
      if (!res.ok) throw new Error('Failed to load report')
      const data = await res.json()
      setReportContent(data.body ?? data.content ?? JSON.stringify(data, null, 2))
    } catch {
      setReportContent('Failed to load report content.')
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
      <div className="report-viewer">
        <div className="report-viewer-header">
          <button className="btn-secondary" onClick={() => setActiveReport(null)}>
            &larr; Back
          </button>
          <div className="report-viewer-meta">
            <h2 className="report-viewer-title">{activeReport.title || activeReport.file}</h2>
            <div className="report-viewer-info">
              <span>{activeReport.folder}</span>
              {activeReport.projectName && (
                <span className="report-viewer-project">{activeReport.projectName}</span>
              )}
              {activeReport.sessionId && (
                <button
                  className="report-viewer-session-link"
                  onClick={() => {
                    window.location.href = `/sessions/${activeReport.sessionId}`
                  }}
                >
                  View Session
                </button>
              )}
            </div>
          </div>
          <button
            className="btn-secondary"
            onClick={async () => {
              const res = await authedFetch(
                `/api/reports/${encodeURIComponent(activeReport.folder)}/${encodeURIComponent(activeReport.file)}/download`,
              )
              if (!res.ok) return
              const blob = await res.blob()
              const url = URL.createObjectURL(blob)
              const a = document.createElement('a')
              a.href = url
              a.download = activeReport.file
              a.click()
              URL.revokeObjectURL(url)
            }}
          >
            Download
          </button>
        </div>
        {loadingContent ? (
          <div className="chat-loading">
            <div className="loading-spinner" />
          </div>
        ) : (
          <div className="report-content">
            <ReactMarkdown remarkPlugins={[remarkGfm]}>{reportContent}</ReactMarkdown>
          </div>
        )}
      </div>
    )
  }

  return (
    <div className="settings-page">
      <h2>Reports</h2>

      {loading && (
        <div className="chat-loading">
          <div className="loading-spinner" />
        </div>
      )}
      {error && <p className="form-error">{error}</p>}

      {!loading && reports.length === 0 && !error && (
        <p style={{ color: 'var(--text3)', fontSize: 'var(--text-sm)' }}>No reports found.</p>
      )}

      {Object.keys(grouped).map((folder) => (
        <section key={folder} className="settings-section">
          <h3 style={{ cursor: 'pointer', userSelect: 'none' }} onClick={() => toggleFolder(folder)}>
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
                    className="report-list-item"
                    onClick={() => viewReport(r)}
                  >
                    <div className="folder-info">
                      <strong>{r.title || r.file}</strong>
                      <span className="folder-path">{r.date}</span>
                    </div>
                    {r.projectName && <span className="report-list-project">{r.projectName}</span>}
                  </button>
                  <button
                    className="btn-secondary"
                    style={{ fontSize: 'var(--text-xs)', padding: '4px 10px' }}
                    onClick={async (e) => {
                      e.stopPropagation()
                      const res = await authedFetch(
                        `/api/reports/${encodeURIComponent(r.folder)}/${encodeURIComponent(r.file)}/download`,
                      )
                      if (!res.ok) return
                      const blob = await res.blob()
                      const url = URL.createObjectURL(blob)
                      const a = document.createElement('a')
                      a.href = url
                      a.download = r.file
                      a.click()
                      URL.revokeObjectURL(url)
                    }}
                  >
                    Download
                  </button>
                </div>
              ))}
            </div>
          )}
        </section>
      ))}
    </div>
  )
}
