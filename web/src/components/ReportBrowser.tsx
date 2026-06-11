import { useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import { useReportsStore, type ReportEntry } from '../store/reports'
import List from './List'
import ListViewHeader from './ListViewHeader'
import SafeMarkdown from './SafeMarkdown'

export default function ReportBrowser() {
  const reports = useReportsStore((s) => s.reports)
  const loading = useReportsStore((s) => s.loading)
  const error = useReportsStore((s) => s.error)
  const fetchReports = useReportsStore((s) => s.fetchReports)

  // Folders expanded by default once reports load; track which the user
  // has explicitly collapsed so a re-fetch doesn't undo their action.
  const [collapsedFolders, setCollapsedFolders] = useState<Set<string>>(new Set())
  const allFolders = new Set(reports.map((r) => r.folder))
  const expandedFolders = new Set([...allFolders].filter((f) => !collapsedFolders.has(f)))

  const [activeReport, setActiveReport] = useState<{
    folder: string
    file: string
    title?: string
    sessionId?: string
    projectName?: string
  } | null>(null)
  const [reportContent, setReportContent] = useState('')
  const [loadingContent, setLoadingContent] = useState(false)

  useEffect(() => {
    fetchReports()
  }, [fetchReports])

  const toggleFolder = (folder: string) => {
    setCollapsedFolders((prev) => {
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

  const downloadReport = async (folder: string, file: string) => {
    const res = await authedFetch(
      `/api/reports/${encodeURIComponent(folder)}/${encodeURIComponent(file)}/download`,
    )
    if (!res.ok) return
    const blob = await res.blob()
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = file
    a.click()
    URL.revokeObjectURL(url)
  }

  const grouped: Record<string, ReportEntry[]> = {}
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
            onClick={() => downloadReport(activeReport.folder, activeReport.file)}
          >
            Download
          </button>
        </div>
        {loadingContent ? (
          <div className="chat-loading">
            <div className="loading-spinner" />
          </div>
        ) : (
          <SafeMarkdown className="report-content">{reportContent}</SafeMarkdown>
        )}
      </div>
    )
  }

  return (
    <div className="list-view">
      <ListViewHeader title="Reports" />
      <div className="list-view-body">
        {loading && (
          <div className="chat-loading">
            <div className="loading-spinner" />
          </div>
        )}
        {error && <p className="form-error">{error}</p>}

        {!loading && reports.length === 0 && !error && (
          <div className="list-view-empty">
            <p>No reports found</p>
          </div>
        )}

        {Object.keys(grouped).map((folder) => (
          <section key={folder} className="report-group">
            <button
              type="button"
              className="report-group-title"
              onClick={() => toggleFolder(folder)}
              aria-expanded={expandedFolders.has(folder)}
            >
              <span className="report-group-chevron" aria-hidden="true">
                {expandedFolders.has(folder) ? '▼' : '▶'}
              </span>
              {folder}
              <span className="report-group-count">{grouped[folder].length}</span>
            </button>
            {expandedFolders.has(folder) && (
              <List
                items={grouped[folder]}
                getKey={(r) => `${r.folder}/${r.file}`}
                onActivate={viewReport}
                bodyClassName="list-view-rows"
                getMenuItems={(r) => [
                  {
                    label: 'Download',
                    onSelect: () => downloadReport(r.folder, r.file),
                  },
                ]}
                renderItem={(r) => (
                  <>
                    <span className="list-view-name">{r.title || r.file}</span>
                    <span className="list-view-meta">
                      {r.projectName && <span className="list-view-tag">{r.projectName}</span>}
                      <span className="list-view-time">{r.date}</span>
                    </span>
                  </>
                )}
              />
            )}
          </section>
        ))}
      </div>
    </div>
  )
}
