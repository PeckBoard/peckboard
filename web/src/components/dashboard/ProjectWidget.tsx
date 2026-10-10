import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react'
import { authedFetch } from '../../store/auth'
import { useProjectsStore } from '../../store/projects'
import { formatRelativeTime } from '../../lib/review'
import { MenuButton, type MenuItem } from '../Dropdown'
import WidgetFrame from './WidgetFrame'
import type { WidgetContext } from './WidgetGrid'

/** `GET /api/projects/{id}/summary`. */
export interface ProjectSummary {
  id: string
  name: string
  status: string
  pause_reason: string | null
  workflow: string
  steps: { step: string; count: number }[]
  total_cards: number
  blocked_cards: number
  active: {
    card_id: string
    title: string
    step: string
    worker_session_id: string | null
    session_running: boolean
  }[]
  worker_count: number
  spend_today_usd: number
  last_activity_at: string | null
}

const POLL_MS = 30_000
/** Coalesces bursts of card events (a worker moving several cards). */
const REFRESH_DEBOUNCE_MS = 400
/** Categorical series slots (index.css `--chart-*`, validated as a set).
 *  Assigned by step ORDER so a step keeps its colour as counts change;
 *  steps past the sixth fold into "Other" rather than reuse a hue. */
const SERIES = 6

function stepLabel(step: string): string {
  const s = step.replace(/[_-]+/g, ' ').trim()
  return s ? s[0].toUpperCase() + s.slice(1) : step
}

function seriesColor(index: number): string {
  return index < SERIES ? `var(--chart-${index + 1})` : 'var(--text3)'
}

function useProjectSummary(projectId: string) {
  const [data, setData] = useState<ProjectSummary | null>(null)
  const [error, setError] = useState<{ message: string; notFound: boolean } | null>(null)
  const seq = useRef(0)

  const load = useCallback(async () => {
    const mine = ++seq.current
    try {
      const res = await authedFetch(`/api/projects/${encodeURIComponent(projectId)}/summary`)
      if (!res.ok) {
        const body = await res.json().catch(() => null)
        throw Object.assign(
          new Error((body && typeof body.error === 'string' && body.error) || 'Failed to load'),
          { notFound: res.status === 404 },
        )
      }
      const json = (await res.json()) as ProjectSummary
      if (mine !== seq.current) return
      setData(json)
      setError(null)
    } catch (e) {
      if (mine !== seq.current) return
      setError({
        message: e instanceof Error ? e.message : 'Failed to load',
        notFound: !!(e as { notFound?: boolean }).notFound,
      })
    }
  }, [projectId])

  useEffect(() => {
    let timer: number | null = null
    const soon = (delay = REFRESH_DEBOUNCE_MS) => {
      if (timer !== null) window.clearTimeout(timer)
      timer = window.setTimeout(() => {
        timer = null
        void load()
      }, delay)
    }
    soon(0)
    // Same WS broadcasts the Kanban board listens to (store/ws.ts).
    const onCard = (e: Event) => {
      const d = (e as CustomEvent).detail?.data
      if (d?.card?.project_id === projectId) soon()
    }
    const onProject = (e: Event) => {
      if ((e as CustomEvent).detail?.data?.project?.id === projectId) soon()
    }
    const onDelete = (e: Event) => {
      if ((e as CustomEvent).detail?.data?.projectId === projectId) soon()
    }
    window.addEventListener('peckboard:card-update', onCard)
    window.addEventListener('peckboard:project-update', onProject)
    window.addEventListener('peckboard:card-delete', onDelete)
    // Fallback for anything without a broadcast (spend, session liveness).
    const poll = window.setInterval(() => {
      if (document.visibilityState === 'visible') void load()
    }, POLL_MS)
    return () => {
      window.removeEventListener('peckboard:card-update', onCard)
      window.removeEventListener('peckboard:project-update', onProject)
      window.removeEventListener('peckboard:card-delete', onDelete)
      window.clearInterval(poll)
      if (timer !== null) window.clearTimeout(timer)
    }
  }, [projectId, load])

  return { data, error, reload: load }
}

/** Searchable single-choice project picker (combobox over the projects store). */
export function ProjectPicker({
  onPick,
  label,
  testId,
  exclude,
  className,
}: {
  onPick: (id: string, name: string) => void
  label: string
  testId: string
  exclude?: Set<string>
  className?: string
}) {
  const projects = useProjectsStore((s) => s.projects)
  const loaded = useProjectsStore((s) => s.projectsLoaded)
  const fetchProjects = useProjectsStore((s) => s.fetchProjects)
  const items: MenuItem[] = projects
    .filter((p) => !exclude?.has(p.id))
    .map((p) => ({
      label: p.name,
      hint: p.status === 'paused' ? 'paused' : undefined,
      searchText: p.id,
      testId: `${testId}-option-${p.id}`,
      onSelect: () => onPick(p.id, p.name),
    }))
  return (
    <MenuButton
      items={items}
      searchable
      searchPlaceholder="Search projects…"
      searchTestId={`${testId}-search`}
      emptyLabel={loaded ? 'No projects' : 'Loading projects…'}
      listLabel="Projects"
      haspopup="listbox"
      ariaLabel={label}
      triggerClassName={className ?? 'btn-secondary btn-sm'}
      testId={testId}
      minWidth={260}
      onOpen={() => void fetchProjects()}
    >
      {label}
    </MenuButton>
  )
}

function StatusPill({ status, reason }: { status: string; reason: string | null }) {
  const tone = status === 'active' ? 'ok' : status === 'paused' ? 'warn' : 'neutral'
  return (
    <span
      className={`project-widget-pill project-widget-pill-${tone}`}
      title={reason ?? undefined}
      data-testid="project-widget-status"
      data-status={status}
    >
      {stepLabel(status)}
    </span>
  )
}

function StepsBar({ steps, total }: { steps: ProjectSummary['steps']; total: number }) {
  const segments = steps.map((s, i) => ({ ...s, color: seriesColor(i) }))
  return (
    <div className="project-widget-steps" data-testid="project-widget-steps">
      <div
        className="project-widget-bar"
        role="img"
        aria-label={
          total === 0
            ? 'No cards'
            : segments
                .filter((s) => s.count > 0)
                .map((s) => `${stepLabel(s.step)} ${s.count}`)
                .join(', ')
        }
      >
        {total === 0 ? (
          <span className="project-widget-bar-empty" />
        ) : (
          segments
            .filter((s) => s.count > 0)
            .map((s) => (
              <span
                key={s.step}
                className="project-widget-seg"
                style={{ flexGrow: s.count, background: s.color }}
                title={`${stepLabel(s.step)}: ${s.count} (${Math.round((s.count / total) * 100)}%)`}
                data-step={s.step}
              />
            ))
        )}
      </div>
      <ul className="project-widget-legend">
        {segments.map((s) => (
          <li
            key={s.step}
            className={s.count === 0 ? 'project-widget-legend-zero' : undefined}
            data-step={s.step}
          >
            <span className="project-widget-swatch" style={{ background: s.color }} />
            <span className="project-widget-legend-label">{stepLabel(s.step)}</span>
            <span className="project-widget-legend-count">{s.count}</span>
          </li>
        ))}
      </ul>
    </div>
  )
}

function Stat({ label, value, tone }: { label: string; value: ReactNode; tone?: 'danger' }) {
  return (
    <div className={`project-widget-stat${tone ? ` project-widget-stat-${tone}` : ''}`}>
      <span className="project-widget-stat-value">{value}</span>
      <span className="project-widget-stat-label">{label}</span>
    </div>
  )
}

/** Project status at a glance: step distribution, counts, active workers,
 *  today's spend. Live via card/project WS events + a 30s poll. */
export default function ProjectWidget({
  widgetId,
  projectId,
  fallbackName,
  menuItems,
  ctx,
  onOpenProject,
  onOpenSession,
  pickerSlot,
}: {
  widgetId: string
  projectId: string
  /** Name from the view's `projects` meta, shown until the summary lands. */
  fallbackName?: string
  menuItems: MenuItem[]
  ctx: WidgetContext
  onOpenProject: (id: string) => void
  onOpenSession?: (sessionId: string) => void
  /** Pickers offered when the project is gone. */
  pickerSlot: ReactNode
}) {
  const { data, error, reload } = useProjectSummary(projectId)
  const name = data?.name ?? fallbackName ?? 'Project'
  // Re-render each minute so "last activity" stays honest between fetches.
  const [, tick] = useState(0)
  useEffect(() => {
    const t = window.setInterval(() => tick((n) => n + 1), 60_000)
    return () => window.clearInterval(t)
  }, [])

  let body: ReactNode
  if (error?.notFound) {
    body = (
      <div className="split-empty-leaf" data-testid="project-widget-missing">
        <p>Project not found — it may have been deleted.</p>
        <div className="view-terminal-closed-actions">{pickerSlot}</div>
      </div>
    )
  } else if (!data) {
    body = error ? (
      <div className="split-empty-leaf" role="alert">
        <p>{error.message}</p>
        <button type="button" className="btn-secondary btn-sm" onClick={() => void reload()}>
          Retry
        </button>
      </div>
    ) : (
      <div className="project-widget-loading" aria-busy="true">
        <span className="project-widget-skeleton" />
        <span className="project-widget-skeleton" />
        <span className="project-widget-skeleton project-widget-skeleton-short" />
      </div>
    )
  } else {
    body = (
      <div className="project-widget" data-testid="project-widget" data-project-id={projectId}>
        {data.status === 'paused' && data.pause_reason && (
          <p className="project-widget-reason" data-testid="project-widget-pause-reason">
            {data.pause_reason}
          </p>
        )}
        <div className="project-widget-stats">
          <Stat label="Cards" value={data.total_cards} />
          <Stat
            label="Blocked"
            value={data.blocked_cards}
            tone={data.blocked_cards > 0 ? 'danger' : undefined}
          />
          <Stat label="Workers" value={data.worker_count} />
          <Stat label="Spend today" value={`$${data.spend_today_usd.toFixed(2)}`} />
        </div>
        <StepsBar steps={data.steps} total={data.total_cards} />
        <div className="project-widget-section">
          <h3 className="project-widget-heading">Active cards</h3>
          {data.active.length === 0 ? (
            <p className="project-widget-none">No cards have a worker right now.</p>
          ) : (
            <ul className="project-widget-active" data-testid="project-widget-active">
              {data.active.map((c) => {
                const sid = c.worker_session_id
                const open = sid && onOpenSession ? () => onOpenSession(sid) : undefined
                const inner = (
                  <>
                    <span
                      className={`project-widget-dot${c.session_running ? ' running' : ''}`}
                      role="img"
                      aria-label={c.session_running ? 'Worker running' : 'Worker idle'}
                    />
                    <span className="project-widget-card-title" title={c.title}>
                      {c.title}
                    </span>
                    <span className="project-widget-card-step">{stepLabel(c.step)}</span>
                  </>
                )
                return (
                  <li key={c.card_id} data-card-id={c.card_id}>
                    {open ? (
                      <button
                        type="button"
                        className="project-widget-card"
                        title="Open worker session"
                        onClick={open}
                      >
                        {inner}
                      </button>
                    ) : (
                      <div className="project-widget-card">{inner}</div>
                    )}
                  </li>
                )
              })}
            </ul>
          )}
        </div>
        <p className="project-widget-foot">
          {data.last_activity_at
            ? `Last activity ${formatRelativeTime(data.last_activity_at)}`
            : 'No activity yet'}
        </p>
      </div>
    )
  }

  return (
    <WidgetFrame
      kind="project"
      widgetId={widgetId}
      title={name}
      onTitleClick={() => onOpenProject(projectId)}
      titleTestId="project-widget-title"
      statusSlot={data && <StatusPill status={data.status} reason={data.pause_reason} />}
      menuItems={menuItems}
      ctx={ctx}
      dataAttrs={{ 'data-project-id': projectId }}
    >
      {body}
    </WidgetFrame>
  )
}
