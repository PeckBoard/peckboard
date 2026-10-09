import { useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import type { Card, CardSessionRun } from '../types/api'
import type { MenuItem } from './Dropdown'
import List from './List'
import Modal from './Modal'
import { STEPS } from './kanban/utils'

interface CardSessionsModalProps {
  projectId: string
  card: Card
  onClose: () => void
  /** Open a run's transcript (the board's session navigation). */
  onOpenSession: (sessionId: string) => void
}

/** Human label per run outcome; unknown outcomes render verbatim. */
const OUTCOME_LABELS: Record<string, string> = {
  advanced: 'Advanced',
  finished: 'Finished',
  reviewed: 'Passed review',
  changes_requested: 'Changes requested',
  stopped: 'Stopped',
  moved: 'Moved',
  wont_do: "Won't do",
  crashed: 'Crashed',
  superseded: 'Superseded',
}

/** Chip tone per outcome: green = the run did its job, amber = sent back,
 *  red = broke. Everything else stays neutral. */
function outcomeTone(outcome: string): 'good' | 'warn' | 'bad' | 'neutral' {
  if (outcome === 'advanced' || outcome === 'finished' || outcome === 'reviewed') return 'good'
  if (outcome === 'changes_requested') return 'warn'
  if (outcome === 'crashed') return 'bad'
  return 'neutral'
}

/** "1h 4m" / "3m 12s" / "8s". */
function formatDuration(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000))
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  if (h > 0) return `${h}h ${m}m`
  if (m > 0) return `${m}m ${s % 60}s`
  return `${s}s`
}

function formatTime(iso: string): string {
  const d = new Date(iso)
  return Number.isNaN(d.getTime())
    ? iso
    : d.toLocaleString(undefined, {
        month: 'short',
        day: 'numeric',
        hour: '2-digit',
        minute: '2-digit',
      })
}

/** Run summaries are markdown, but the row is a button (phrasing content
 *  only), so show them as plain text: drop emphasis / code / heading /
 *  list markers and keep the line breaks. */
function plainSummary(md: string): string {
  return md
    .replace(/(\*\*|__|`)/g, '')
    .replace(/^\s{0,3}(#{1,6}\s+|[-*+]\s+|>\s?)/gm, '')
    .replace(/\n{2,}/g, '\n')
    .trim()
}

/**
 * Every agent run recorded on a card — implementation and review — newest
 * first, with each run's outcome and summary. Opening a row jumps to that
 * run's (sealed, read-only) transcript.
 */
export default function CardSessionsModal({
  projectId,
  card,
  onClose,
  onOpenSession,
}: CardSessionsModalProps) {
  const [runs, setRuns] = useState<CardSessionRun[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set())
  const [now, setNow] = useState(() => Date.now())

  useEffect(() => {
    let cancelled = false
    authedFetch(`/api/projects/${projectId}/cards/${card.id}/sessions`)
      .then(async (res) => {
        if (!res.ok) throw new Error(`Failed to load sessions (${res.status})`)
        const data = (await res.json()) as { sessions: CardSessionRun[] }
        if (!cancelled) setRuns(data.sessions)
      })
      .catch((err: unknown) => {
        if (!cancelled) setError(err instanceof Error ? err.message : 'Failed to load sessions')
      })
    return () => {
      cancelled = true
    }
  }, [projectId, card.id])

  // Tick the live duration of a still-running run.
  const anyLive = runs?.some((r) => !r.ended_at) ?? false
  useEffect(() => {
    if (!anyLive) return
    const t = setInterval(() => setNow(Date.now()), 1000)
    return () => clearInterval(t)
  }, [anyLive])

  const toggleExpanded = (id: string) =>
    setExpanded((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })

  const open = (run: CardSessionRun) => {
    if (run.session_exists) onOpenSession(run.session_id)
  }

  const menuFor = (run: CardSessionRun): MenuItem[] => [
    {
      label: 'Open transcript',
      hint: run.session_exists ? undefined : 'Transcript deleted',
      disabled: !run.session_exists,
      onSelect: () => open(run),
    },
    {
      label: expanded.has(run.id) ? 'Collapse summary' : 'Expand summary',
      hidden: !run.summary,
      onSelect: () => toggleExpanded(run.id),
    },
  ]

  const renderRun = (run: CardSessionRun) => {
    const stepLabel = STEPS.find((s) => s.key === run.step)?.label ?? run.step
    const started = new Date(run.started_at).getTime()
    const ended = run.ended_at ? new Date(run.ended_at).getTime() : now
    const duration = Number.isNaN(started) || Number.isNaN(ended) ? null : ended - started
    return (
      <span
        className={`card-run${run.session_exists ? '' : ' card-run--gone'}`}
        data-testid="card-run-row"
        data-role={run.role}
      >
        <span className="card-run-head">
          <span className="card-run-step">{stepLabel}</span>
          <span className={`card-run-role card-run-role--${run.role}`}>
            {run.role === 'review' ? 'Review' : 'Implementation'}
          </span>
          {run.model && <span className="card-run-model">{run.model}</span>}
          {run.outcome ? (
            <span
              className={`card-run-outcome card-run-outcome--${outcomeTone(run.outcome)}`}
              data-testid="card-run-outcome"
            >
              {OUTCOME_LABELS[run.outcome] ?? run.outcome}
            </span>
          ) : (
            !run.ended_at && (
              <span className="card-run-outcome card-run-outcome--live">Running</span>
            )
          )}
          {run.sealed && (
            <span className="card-run-sealed" title="Read-only: this run is finished">
              Sealed
            </span>
          )}
        </span>
        <span className="card-run-time">
          <span title={new Date(run.started_at).toLocaleString()}>
            {formatTime(run.started_at)}
          </span>
          {' → '}
          {run.ended_at ? (
            <span title={new Date(run.ended_at).toLocaleString()}>{formatTime(run.ended_at)}</span>
          ) : (
            <span>running</span>
          )}
          {duration !== null && (
            <span className="card-run-duration"> · {formatDuration(duration)}</span>
          )}
          {!run.session_exists && <span className="card-run-deleted"> · Transcript deleted</span>}
        </span>
        {run.summary && (
          <span
            className={`card-run-summary${expanded.has(run.id) ? ' expanded' : ''}`}
            data-testid="card-run-summary"
            title={expanded.has(run.id) ? undefined : plainSummary(run.summary)}
          >
            {plainSummary(run.summary)}
          </span>
        )}
      </span>
    )
  }

  return (
    <Modal onClose={onClose} className="card-sessions-modal" data-testid="card-sessions-modal">
      <h2>Sessions — {card.title}</h2>
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
      {!runs && !error && <p className="modal-subtitle">Loading…</p>}
      {runs && (
        <List<CardSessionRun>
          items={runs}
          getKey={(r) => r.id}
          bodyClassName="list-view-rows card-sessions-list"
          onActivate={open}
          getMenuItems={menuFor}
          renderItem={renderRun}
          emptyState={
            <div className="list-view-empty" data-testid="card-sessions-empty">
              <p>No agent has worked this card yet.</p>
            </div>
          }
        />
      )}
      <div className="card-detail-actions">
        <button className="btn-secondary" onClick={onClose}>
          Close
        </button>
      </div>
    </Modal>
  )
}
