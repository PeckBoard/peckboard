import { useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import { useReportsStore, type ReportEntry } from '../store/reports'
import List from './List'
import ListViewHeader from './ListViewHeader'

interface ReportBrowserProps {
  /** Open a single report by its (folder, file) pair. Drives the
   *  URL navigation in App.tsx, which in turn renders [[ReportView]]
   *  and opens a tab via the cross-device tab strip. */
  onOpenReport: (folder: string, file: string) => void
}

/**
 * List / index of report markdown files. Clicking a row hands off to
 * [[App]] via `onOpenReport`, which navigates to `/reports/:folder/:file`
 * (opens a tab and mounts [[ReportView]]). The browser itself never
 * holds the active-report state — that lives in the URL so the report
 * can be deep-linked and persists in the tab strip across devices.
 */
export default function ReportBrowser({ onOpenReport }: ReportBrowserProps) {
  const reports = useReportsStore((s) => s.reports)
  const loading = useReportsStore((s) => s.loading)
  const error = useReportsStore((s) => s.error)
  const fetchReports = useReportsStore((s) => s.fetchReports)

  // Folders expanded by default once reports load; track which the user
  // has explicitly collapsed so a re-fetch doesn't undo their action.
  const [collapsedFolders, setCollapsedFolders] = useState<Set<string>>(new Set())
  const allFolders = new Set(reports.map((r) => r.folder))
  const expandedFolders = new Set([...allFolders].filter((f) => !collapsedFolders.has(f)))

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
                onActivate={(r) => onOpenReport(r.folder, r.file)}
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
