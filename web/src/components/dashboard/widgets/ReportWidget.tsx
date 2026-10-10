import SafeMarkdown from '../../SafeMarkdown'
import { chatMarkdownComponents } from '../../chat/markdown'
import { highlightPlugins } from '../../markdownHighlight'
import { formatRelativeTime } from '../../../lib/review'
import type { ReportEntry } from '../../../store/reports'
import WidgetFrame from '../WidgetFrame'
import { DashEmpty, DashError, DashLoading } from './DashParts'
import { useDashboardData } from './useDashboardData'
import type { InfoWidgetProps } from './types'
import '../../../styles/dashboard-info.css'

interface ReportDetail extends ReportEntry {
  body?: string
  content?: string
}

/** `"<folder>/<file>"` → parts; the file name never holds a slash. */
function splitReportRef(ref: string): { folder: string; file: string } | null {
  const i = ref.lastIndexOf('/')
  if (i <= 0 || i === ref.length - 1) return null
  return { folder: ref.slice(0, i), file: ref.slice(i + 1) }
}

/** Newest report by frontmatter date (unparseable dates sink). */
function latestReport(list: ReportEntry[]): ReportEntry | null {
  let best: ReportEntry | null = null
  let bestT = -Infinity
  for (const r of list) {
    const t = new Date(r.date).getTime()
    const v = Number.isNaN(t) ? -Infinity : t
    if (!best || v > bestT) {
      best = r
      bestT = v
    }
  }
  return best
}

/** A pinned report (`reportRef`), or the latest one overall when unpinned,
 *  rendered with the report viewer's markdown components. */
export default function ReportWidget({
  widget,
  ctx,
  menuItems,
  onOpenSession,
  onOpenReport,
}: InfoWidgetProps) {
  const pinned = widget.reportRef ? splitReportRef(widget.reportRef) : null
  const list = useDashboardData<ReportEntry[] | { reports?: ReportEntry[] }>(
    pinned ? null : '/api/reports',
    null,
    [],
  )
  const latest = list.data
    ? latestReport(Array.isArray(list.data) ? list.data : (list.data.reports ?? []))
    : null
  const target = pinned ?? (latest ? { folder: latest.folder, file: latest.file } : null)
  const detail = useDashboardData<ReportDetail>(
    target
      ? `/api/reports/${encodeURIComponent(target.folder)}/${encodeURIComponent(target.file)}`
      : null,
    null,
    [],
  )
  const d = detail.data
  const title = d?.title || latest?.title || target?.file || 'Report'
  const body = d ? (d.body ?? d.content ?? '') : ''

  let content
  if (!pinned && list.error) {
    content = <DashError message={list.error} onRetry={() => void list.reload()} />
  } else if (!pinned && list.data && !latest) {
    content = (
      <DashEmpty testId="dash-report-empty">
        <p>No reports yet</p>
        <p className="form-hint">Reports agents write appear here.</p>
      </DashEmpty>
    )
  } else if (detail.error) {
    content = <DashError message={detail.error} onRetry={() => void detail.reload()} />
  } else if (!d) {
    content = <DashLoading />
  } else {
    content = (
      <div className="dash-scroll" data-testid="dash-report-body">
        <div className="dash-report-meta">
          <span className="dash-pill">{pinned ? 'Pinned' : 'Latest'}</span>
          {d.project_name && <span>{d.project_name}</span>}
          {d.date && <span title={d.date}>{formatRelativeTime(d.date)}</span>}
          {d.session_id && (
            <button
              type="button"
              className="dash-link"
              onClick={() => onOpenSession(d.session_id!)}
              title={d.session_name ? `Open ${d.session_name}` : 'Open session'}
            >
              {d.session_name || 'Session'}
            </button>
          )}
        </div>
        <SafeMarkdown
          className="report-content dash-markdown"
          rehypePlugins={highlightPlugins}
          components={chatMarkdownComponents}
        >
          {body}
        </SafeMarkdown>
      </div>
    )
  }

  return (
    <WidgetFrame
      kind="report"
      widgetId={widget.id}
      title={title}
      titleTestId="dash-report-title"
      onTitleClick={
        target && onOpenReport ? () => onOpenReport(target.folder, target.file) : undefined
      }
      menuItems={[
        {
          label: 'Open report',
          testId: 'widget-open-tab',
          hidden: !target || !onOpenReport,
          onSelect: () => target && onOpenReport?.(target.folder, target.file),
        },
        ...menuItems,
      ]}
      ctx={ctx}
    >
      {content}
    </WidgetFrame>
  )
}
