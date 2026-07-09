import { useEffect, useMemo, useState } from 'react'
import { authedFetch } from '../store/auth'
import {
  useReportsStore,
  filterAndSortReports,
  type ReportEntry,
  type ReportSortOrder,
} from '../store/reports'
import List from './List'
import ListViewHeader from './ListViewHeader'

interface ReportBrowserProps {
  /** Open a single report by its (folder, file) pair. Drives the
   *  URL navigation in App.tsx, which in turn renders [[ReportView]]
   *  and opens a tab via the cross-device tab strip. */
  onOpenReport: (folder: string, file: string) => void
}

/** RFC3339 → locale string; falls back to the raw value when unparseable
 *  so a malformed date still shows *something*. */
function formatReportDate(date: string): string {
  const d = new Date(date)
  return Number.isNaN(d.getTime()) ? date : d.toLocaleString()
}

/**
 * List / index of report markdown files. Clicking a row hands off to
 * [[App]] via `onOpenReport`, which navigates to `/reports/:folder/:file`
 * (opens a tab and mounts [[ReportView]]). The browser itself never
 * holds the active-report state — that lives in the URL so the report
 * can be deep-linked and persists in the tab strip across devices.
 *
 * The toolbar (search box, session/project filters, sort toggle) filters
 * and sorts client-side over the already-fetched metadata via
 * [[filterAndSortReports]]; the visible rows are then grouped by their
 * date folder in the resulting (sorted) order.
 */
export default function ReportBrowser({ onOpenReport }: ReportBrowserProps) {
  const reports = useReportsStore((s) => s.reports)
  const loading = useReportsStore((s) => s.loading)
  const error = useReportsStore((s) => s.error)
  const fetchReports = useReportsStore((s) => s.fetchReports)

  const [query, setQuery] = useState('')
  const [sessionId, setSessionId] = useState('')
  const [projectName, setProjectName] = useState('')
  const [order, setOrder] = useState<ReportSortOrder>('newest')

  // Folders expanded by default once reports load; track which the user
  // has explicitly collapsed so a re-fetch doesn't undo their action.
  const [collapsedFolders, setCollapsedFolders] = useState<Set<string>>(new Set())

  useEffect(() => {
    fetchReports()
  }, [fetchReports])

  // Distinct sessions / projects present, for the filter dropdowns.
  const sessionOptions = useMemo(
    () => [...new Set(reports.map((r) => r.session_id).filter((s): s is string => !!s))].sort(),
    [reports],
  )
  const projectOptions = useMemo(
    () => [...new Set(reports.map((r) => r.project_name).filter((p): p is string => !!p))].sort(),
    [reports],
  )

  const visible = useMemo(
    () => filterAndSortReports(reports, { query, sessionId, projectName, order }),
    [reports, query, sessionId, projectName, order],
  )

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

  // Group the visible (already filtered + sorted) reports by date folder.
  // Insertion order follows the sorted list, so folders render newest-first
  // (or oldest-first) to match the chosen sort order.
  const grouped: Record<string, ReportEntry[]> = {}
  for (const r of visible) {
    if (!grouped[r.folder]) grouped[r.folder] = []
    grouped[r.folder].push(r)
  }
  const folderOrder = Object.keys(grouped)
  const expandedFolders = new Set(folderOrder.filter((f) => !collapsedFolders.has(f)))

  const hasFilter = query.trim() !== '' || sessionId !== '' || projectName !== ''

  const toolbar = (
    <div className="report-toolbar">
      <input
        type="search"
        className="report-search"
        placeholder="Search reports…"
        value={query}
        onChange={(e) => setQuery(e.target.value)}
        aria-label="Search reports"
      />
      {sessionOptions.length > 0 && (
        <select
          className="report-filter"
          value={sessionId}
          onChange={(e) => setSessionId(e.target.value)}
          aria-label="Filter by session"
        >
          <option value="">All sessions</option>
          {sessionOptions.map((s) => (
            <option key={s} value={s}>
              {s.slice(0, 8)}
            </option>
          ))}
        </select>
      )}
      {projectOptions.length > 0 && (
        <select
          className="report-filter"
          value={projectName}
          onChange={(e) => setProjectName(e.target.value)}
          aria-label="Filter by project"
        >
          <option value="">All projects</option>
          {projectOptions.map((p) => (
            <option key={p} value={p}>
              {p}
            </option>
          ))}
        </select>
      )}
      <button
        type="button"
        className="report-sort-toggle"
        onClick={() => setOrder((o) => (o === 'newest' ? 'oldest' : 'newest'))}
        title={order === 'newest' ? 'Newest first' : 'Oldest first'}
      >
        {order === 'newest' ? 'Newest ↓' : 'Oldest ↑'}
      </button>
    </div>
  )

  return (
    <div className="list-view">
      <ListViewHeader title="Reports" extras={toolbar} />
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

        {!loading && reports.length > 0 && visible.length === 0 && (
          <div className="list-view-empty">
            <p>{hasFilter ? 'No reports match your filters' : 'No reports found'}</p>
          </div>
        )}

        {folderOrder.map((folder) => (
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
                      {r.project_name && <span className="list-view-tag">{r.project_name}</span>}
                      {r.session_name && <span className="list-view-tag">{r.session_name}</span>}
                      <span className="list-view-time">{formatReportDate(r.date)}</span>
                      {r.session_created_at && (
                        <span className="list-view-time">
                          Session created {formatReportDate(r.session_created_at)}
                        </span>
                      )}
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
