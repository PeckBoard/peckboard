import { useEffect, useState } from 'react'
import { authedFetch } from '../../store/auth'
import { useProjectsStore } from '../../store/projects'
import type { Project, ProjectStatusEntry } from '../../types/api'
import { describeActionError } from '../../utils/actionError'
import { formatLimitReset } from '../chat/events'
import { pauseReasonNote, statusActorLabel, statusEntryLabel } from './utils'

/** The board's "paused" banner: who paused the project and when, the reason
 *  if one was given, a Resume button, and the recent pauses and resumes. */
export default function ProjectPausedBanner({ project }: { project: Project }) {
  const change = project.last_status_change
  // Null for a project paused before pauses were recorded.
  const pause = change?.action === 'paused' ? change : null
  const reason = pauseReasonNote(project.pause_reason, change)

  const [resuming, setResuming] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const resume = async () => {
    setResuming(true)
    setError(null)
    try {
      const res = await authedFetch(`/api/projects/${project.id}/resume`, { method: 'POST' })
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      const next = (await res.json()) as Project
      useProjectsStore.setState((s) => ({
        projects: s.projects.map((p) => (p.id === next.id ? next : p)),
      }))
    } catch (e) {
      setError(describeActionError(e, "Couldn't resume the project."))
    } finally {
      setResuming(false)
    }
  }

  const [historyOpen, setHistoryOpen] = useState(false)
  // null = not loaded yet.
  const [history, setHistory] = useState<ProjectStatusEntry[] | null>(null)
  const [historyError, setHistoryError] = useState(false)
  // Refetch whenever a new pause or resume lands while the list is open.
  const changeId = change?.id
  useEffect(() => {
    if (!historyOpen) return
    let stale = false
    authedFetch(`/api/projects/${project.id}/status-log`)
      .then((r) => (r.ok ? r.json() : Promise.reject(new Error(`HTTP ${r.status}`))))
      .then((body: { entries?: ProjectStatusEntry[] }) => {
        if (stale) return
        setHistory(body.entries ?? [])
        setHistoryError(false)
      })
      .catch(() => {
        if (!stale) setHistoryError(true)
      })
    return () => {
      stale = true
    }
  }, [historyOpen, project.id, changeId])

  return (
    <div className="project-pause-banner" role="status" data-testid="project-paused-banner">
      <div className="project-paused-banner-head">
        <span className="project-paused-banner-text">
          <strong>
            {pause
              ? `Project paused by ${statusActorLabel(pause)} · ${formatLimitReset(pause.at)}.`
              : 'Project paused.'}
          </strong>
        </span>
        <span className="project-paused-banner-actions">
          <button
            type="button"
            className="btn-secondary btn-sm"
            aria-expanded={historyOpen}
            onClick={() => setHistoryOpen((o) => !o)}
            data-testid="project-status-history-toggle"
          >
            {historyOpen ? 'Hide history' : 'History'}
          </button>
          <button
            type="button"
            className="btn-secondary btn-sm"
            disabled={resuming}
            onClick={() => void resume()}
            data-testid="project-paused-resume"
          >
            {resuming ? 'Resuming…' : 'Resume'}
          </button>
        </span>
      </div>
      {reason && <div className="project-paused-banner-reason">{reason}</div>}
      {error && <div role="alert">{error}</div>}
      {historyOpen && (
        <div className="project-status-history" data-testid="project-status-history">
          {historyError ? (
            <span role="alert">Couldn’t load the history.</span>
          ) : history === null ? (
            <span>Loading…</span>
          ) : history.length === 0 ? (
            <span>No pauses or resumes recorded yet.</span>
          ) : (
            <ul>
              {history.map((entry) => (
                <li key={entry.id} data-testid="project-status-history-row">
                  {statusEntryLabel(entry)} — {formatLimitReset(entry.at)}
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </div>
  )
}
