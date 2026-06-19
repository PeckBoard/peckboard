import { useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import SafeMarkdown from './SafeMarkdown'

interface ReportViewProps {
  folder: string
  file: string
  /** Navigate back to the report browser. */
  onBack: () => void
  /** Open a session by id — used by the "View Session" link in the
   *  report frontmatter, which jumps to the agent that produced it. */
  onOpenSession: (sessionId: string) => void
}

interface ReportMeta {
  folder: string
  file: string
  title: string
  date: string
  session_id?: string
  project_name?: string
}

/** Discriminated union for the report fetch lifecycle. Single state
 *  variable so the `useEffect` only ever calls setState inside its
 *  async callback (`react-hooks/set-state-in-effect` would fail a
 *  multi-setState reset pattern at the top of the effect). */
type FetchState =
  | { status: 'loading'; folder: string; file: string }
  | { status: 'ready'; folder: string; file: string; meta: ReportMeta; body: string }
  | { status: 'error'; folder: string; file: string; message: string }

/**
 * Single-report viewer. Route-driven (mounted from App.tsx at
 * `/reports/:folder/:file`) so the report can be opened as a tab and
 * deep-linked across devices via the cross-device tab strip.
 *
 * Companion to [[ReportBrowser]], which is the list / index page.
 */
export default function ReportView({ folder, file, onBack, onOpenSession }: ReportViewProps) {
  const [state, setState] = useState<FetchState>({ status: 'loading', folder, file })

  useEffect(() => {
    let cancelled = false
    const url = `/api/reports/${encodeURIComponent(folder)}/${encodeURIComponent(file)}`
    authedFetch(url)
      .then(async (res) => {
        if (cancelled) return
        if (!res.ok) {
          const message = res.status === 404 ? 'Report not found.' : 'Failed to load report.'
          setState({ status: 'error', folder, file, message })
          return
        }
        const data = (await res.json()) as ReportMeta & { body?: string; content?: string }
        if (cancelled) return
        setState({
          status: 'ready',
          folder,
          file,
          meta: {
            folder: data.folder,
            file: data.file,
            title: data.title,
            date: data.date,
            session_id: data.session_id,
            project_name: data.project_name,
          },
          body: data.body ?? data.content ?? '',
        })
      })
      .catch(() => {
        if (cancelled) return
        setState({ status: 'error', folder, file, message: 'Failed to load report.' })
      })
    return () => {
      cancelled = true
    }
  }, [folder, file])

  // When the route swaps to a different report mid-mount, reset the
  // displayed state to "loading" so we don't flash stale meta from the
  // previous report. Derived from props rather than synchronously
  // setState'd inside the effect.
  const displayed: FetchState =
    state.folder === folder && state.file === file ? state : { status: 'loading', folder, file }

  const downloadReport = async () => {
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

  if (displayed.status === 'error') {
    return (
      <div className="report-viewer">
        <div className="report-viewer-header">
          <button className="btn-secondary" onClick={onBack}>
            &larr; Back
          </button>
        </div>
        <div className="list-view-empty">
          <p>{displayed.message}</p>
        </div>
      </div>
    )
  }

  const meta = displayed.status === 'ready' ? displayed.meta : null
  const body = displayed.status === 'ready' ? displayed.body : ''
  const loading = displayed.status === 'loading'

  return (
    <div className="report-viewer">
      <div className="report-viewer-header">
        <button className="btn-secondary" onClick={onBack}>
          &larr; Back
        </button>
        <div className="report-viewer-meta">
          <h2 className="report-viewer-title">{meta?.title || file}</h2>
          <div className="report-viewer-info">
            <span>{folder}</span>
            {meta?.project_name && (
              <span className="report-viewer-project">{meta.project_name}</span>
            )}
            {meta?.session_id && (
              <button
                className="report-viewer-session-link"
                onClick={() => onOpenSession(meta.session_id!)}
              >
                View Session
              </button>
            )}
          </div>
        </div>
        <button className="btn-secondary" onClick={downloadReport}>
          Download
        </button>
      </div>
      {loading ? (
        <div className="chat-loading">
          <div className="loading-spinner" />
        </div>
      ) : (
        <SafeMarkdown className="report-content">{body}</SafeMarkdown>
      )}
    </div>
  )
}
