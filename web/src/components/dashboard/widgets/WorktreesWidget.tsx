import { useState, type ReactNode } from 'react'
import WidgetFrame from '../WidgetFrame'
import { authedFetch } from '../../../store/auth'
import { formatRelativeTime } from '../../../lib/review'
import type { InfoWidgetProps } from './types'
import { humanize, useDashboardData } from './useDashboardData'
import { DashCount, DashEmpty, DashError, DashHeading, DashLoading, DashNoMatch } from './DashParts'
import { FilterBar, FilterButton, inSet, useWidgetFilters, type FilterDef } from './filters'

const REASONS = ['dirty', 'conflict', 'cleanup_failed', 'worktree_missing']

interface Unmerged {
  card_id: string
  title: string
  project_id: string
  project_name: string
  reason: string
  detail: string | null
  updated_at: string
}

interface Commit {
  project_id: string
  project_name: string
  sha: string
  subject: string
  author: string
  date: string
}

function RetryMerge({ row, onDone }: { row: Unmerged; onDone: () => void }) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const retry = async () => {
    setBusy(true)
    setError(null)
    try {
      const res = await authedFetch(
        `/api/projects/${encodeURIComponent(row.project_id)}/cards/${encodeURIComponent(row.card_id)}/retry-merge`,
        { method: 'POST' },
      )
      const data = (await res.json().catch(() => ({}))) as { error?: string }
      if (!res.ok) throw new Error(data?.error || `HTTP ${res.status}`)
      onDone()
    } catch (e) {
      setError(e instanceof Error ? e.message : "Couldn't merge the worktree.")
    } finally {
      setBusy(false)
    }
  }
  return (
    <>
      <button
        type="button"
        className="btn-secondary btn-sm dash-row-action"
        disabled={busy}
        onClick={() => void retry()}
        data-testid="dash-worktrees-retry"
      >
        {busy ? 'Merging…' : 'Retry merge'}
      </button>
      {error && (
        <span className="dash-inline-error" role="alert" title={error}>
          {error}
        </span>
      )}
    </>
  )
}

/** Git / Worktrees: cards whose worktree failed to merge, plus recent commits. */
export default function WorktreesWidget(props: InfoWidgetProps) {
  const { widget, ctx, menuItems, scopeProjectId, scopeName, onOpenProject } = props
  const { data, error, reload } = useDashboardData<{ unmerged: Unmerged[]; commits: Commit[] }>(
    '/api/dashboard/worktrees',
    scopeProjectId,
    ['card-update', 'card-delete'],
  )
  const {
    filters,
    set,
    clear,
    activeCount,
    open: filterOpen,
    toggle: toggleFilters,
  } = useWidgetFilters(props)
  const authors = [...new Set((data?.commits ?? []).map((c) => c.author))].sort()
  const defs: FilterDef[] = [
    {
      key: 'reason',
      kind: 'chips',
      label: 'Reason',
      options: REASONS.map((r) => ({ value: r, label: humanize(r) })),
    },
    {
      key: 'author',
      kind: 'combo',
      label: 'Author',
      options: authors.map((a) => ({ value: a, label: a })),
    },
  ]
  const unmerged = (data?.unmerged ?? []).filter((u) => inSet(filters.reason, u.reason))
  const commits = (data?.commits ?? []).filter((c) => inSet(filters.author, c.author))

  let body: ReactNode
  if (!data) {
    body = error ? <DashError message={error} onRetry={() => void reload()} /> : <DashLoading />
  } else if (data.unmerged.length === 0 && data.commits.length === 0) {
    body = (
      <DashEmpty testId="dash-worktrees-empty">No unmerged worktrees or recent commits</DashEmpty>
    )
  } else if (unmerged.length === 0 && commits.length === 0) {
    body = <DashNoMatch onClear={clear} />
  } else {
    body = (
      <div className="dash-scroll" data-testid="dash-worktrees-list">
        <section className="dash-group">
          <DashHeading count={unmerged.length}>Needs merge</DashHeading>
          {unmerged.length === 0 ? (
            <p className="project-widget-none">Every worktree is merged.</p>
          ) : (
            <ul className="dash-list">
              {unmerged.map((u) => (
                <li key={u.card_id} className="dash-row dash-row-static" data-card-id={u.card_id}>
                  <span className="dash-pill dash-pill-warn" title={u.detail ?? u.reason}>
                    {humanize(u.reason)}
                  </span>
                  <button
                    type="button"
                    className="dash-row-main dash-link"
                    title={u.detail ?? 'Open project board'}
                    onClick={() => onOpenProject(u.project_id)}
                  >
                    <span className="dash-row-title">{u.title}</span>
                    <span className="dash-row-sub">
                      {[scopeProjectId ? null : u.project_name, formatRelativeTime(u.updated_at)]
                        .filter(Boolean)
                        .join(' · ')}
                    </span>
                  </button>
                  <RetryMerge row={u} onDone={() => void reload()} />
                </li>
              ))}
            </ul>
          )}
        </section>
        <section className="dash-group">
          <DashHeading>Recent commits</DashHeading>
          {commits.length === 0 ? (
            <p className="project-widget-none">No commits found.</p>
          ) : (
            <ul className="dash-list dash-commits">
              {commits.map((c) => (
                <li
                  key={`${c.project_id}-${c.sha}`}
                  className="dash-commit"
                  title={`${c.sha}\n${c.subject}`}
                >
                  <span className="dash-mono dash-sha">{c.sha.slice(0, 7)}</span>
                  <span className="dash-row-title">{c.subject}</span>
                  <span className="dash-row-sub dash-commit-meta">
                    {[scopeProjectId ? null : c.project_name, c.author].filter(Boolean).join(' · ')}
                  </span>
                  <span className="dash-row-time">{formatRelativeTime(c.date)}</span>
                </li>
              ))}
            </ul>
          )}
        </section>
      </div>
    )
  }

  return (
    <WidgetFrame
      kind="worktrees"
      widgetId={widget.id}
      title={scopeName ? `Git / Worktrees · ${scopeName}` : 'Git / Worktrees'}
      statusSlot={
        data ? (
          <>
            {data.unmerged.length > 0 && (
              <DashCount
                n={data.unmerged.length}
                tone="warn"
                label={`${data.unmerged.length} need merge`}
              />
            )}
            <FilterButton activeCount={activeCount} open={filterOpen} onToggle={toggleFilters} />
          </>
        ) : undefined
      }
      menuItems={menuItems}
      ctx={ctx}
    >
      {filterOpen && data && <FilterBar defs={defs} filters={filters} set={set} clear={clear} />}
      {body}
    </WidgetFrame>
  )
}
