import type { ReactNode } from 'react'
import WidgetFrame from '../WidgetFrame'
import { formatRelativeTime } from '../../../lib/review'
import type { InfoWidgetProps } from './types'
import { useDashboardData } from './useDashboardData'
import { DashCount, DashEmpty, DashError, DashLoading } from './DashParts'

type AttentionKind = 'question' | 'plan' | 'blocked' | 'worker_error' | 'unmerged'

interface AttentionItem {
  kind: AttentionKind
  project_id: string | null
  project_name: string | null
  card_id: string | null
  card_title: string | null
  session_id: string | null
  title: string
  detail: string | null
  at: string
}

/** Group order: things waiting on a human answer first. */
const GROUPS: { kind: AttentionKind; label: string; tone: string; icon: ReactNode }[] = [
  {
    kind: 'question',
    label: 'Questions',
    tone: 'accent',
    icon: (
      <>
        <circle cx="8" cy="8" r="6" fill="none" stroke="currentColor" strokeWidth="1.4" />
        <path
          d="M6.3 6.3a1.8 1.8 0 113 1.3c-.7.4-1.3.8-1.3 1.6M8 11.2v.1"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.4"
          strokeLinecap="round"
        />
      </>
    ),
  },
  {
    kind: 'plan',
    label: 'Plans to review',
    tone: 'accent',
    icon: (
      <path
        d="M4 2.5h6l2.5 2.5v8.5H4zM6 7h4.5M6 9.5h4.5M6 12h2.5"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.3"
      />
    ),
  },
  {
    kind: 'worker_error',
    label: 'Worker errors',
    tone: 'danger',
    icon: (
      <>
        <path d="M8 2l6.5 11.5h-13z" fill="none" stroke="currentColor" strokeWidth="1.3" />
        <path
          d="M8 6.5v3.2M8 11.4v.1"
          stroke="currentColor"
          strokeWidth="1.5"
          strokeLinecap="round"
        />
      </>
    ),
  },
  {
    kind: 'blocked',
    label: 'Blocked cards',
    tone: 'danger',
    icon: (
      <>
        <circle cx="8" cy="8" r="6" fill="none" stroke="currentColor" strokeWidth="1.4" />
        <path d="M3.8 12.2l8.4-8.4" stroke="currentColor" strokeWidth="1.4" />
      </>
    ),
  },
  {
    kind: 'unmerged',
    label: 'Needs merge',
    tone: 'warn',
    icon: (
      <>
        <circle cx="4.5" cy="3.5" r="1.6" fill="none" stroke="currentColor" strokeWidth="1.3" />
        <circle cx="4.5" cy="12.5" r="1.6" fill="none" stroke="currentColor" strokeWidth="1.3" />
        <circle cx="11.5" cy="8" r="1.6" fill="none" stroke="currentColor" strokeWidth="1.3" />
        <path
          d="M4.5 5.1v5.8M4.5 6.5c0 1.5 2 1.5 5.4 1.5"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.3"
        />
      </>
    ),
  },
]

/** Needs Attention: everything waiting on a human, grouped by kind. */
export default function AttentionWidget({
  widget,
  ctx,
  menuItems,
  scopeProjectId,
  scopeName,
  onOpenSession,
  onOpenProject,
}: InfoWidgetProps) {
  const { data, error, reload } = useDashboardData<{ items: AttentionItem[] }>(
    '/api/dashboard/attention',
    scopeProjectId,
    ['card-update', 'card-delete', 'worker-question', 'project-update'],
  )
  const items = data?.items ?? []

  const open = (it: AttentionItem): (() => void) | undefined => {
    const toSession = it.kind === 'question' || it.kind === 'plan' || it.kind === 'worker_error'
    if (toSession && it.session_id) {
      const sid = it.session_id
      return () => onOpenSession(sid)
    }
    if (it.project_id) {
      const pid = it.project_id
      return () => onOpenProject(pid)
    }
    return undefined
  }

  let body: ReactNode
  if (!data) {
    body = error ? <DashError message={error} onRetry={() => void reload()} /> : <DashLoading />
  } else if (items.length === 0) {
    body = (
      <DashEmpty testId="dash-attention-empty">
        <svg
          width="22"
          height="22"
          viewBox="0 0 16 16"
          aria-hidden="true"
          className="dash-empty-ok"
        >
          <circle cx="8" cy="8" r="6.5" fill="none" stroke="currentColor" strokeWidth="1.3" />
          <path d="M5 8.2l2 2 4-4.2" fill="none" stroke="currentColor" strokeWidth="1.5" />
        </svg>
        <span>All clear</span>
      </DashEmpty>
    )
  } else {
    body = (
      <div className="dash-scroll" data-testid="dash-attention-list">
        {GROUPS.map((g) => {
          const rows = items.filter((it) => it.kind === g.kind)
          if (rows.length === 0) return null
          return (
            <section key={g.kind} className="dash-group" data-group={g.kind}>
              <h3 className={`dash-group-head dash-tone-${g.tone}`}>
                <svg width="13" height="13" viewBox="0 0 16 16" aria-hidden="true">
                  {g.icon}
                </svg>
                <span>{g.label}</span>
                <span className="dash-heading-count">{rows.length}</span>
              </h3>
              <ul className="dash-list">
                {rows.map((it, i) => {
                  const onClick = open(it)
                  // Card kinds carry a generic title ("Card blocked") the
                  // group heading already says; lead with the card instead.
                  const cardKind = it.kind !== 'question' && it.kind !== 'plan'
                  const primary = cardKind && it.card_title ? it.card_title : it.title
                  const sub = [
                    it.project_name,
                    it.card_title && it.card_title !== primary ? it.card_title : null,
                  ]
                    .filter(Boolean)
                    .join(' · ')
                  return (
                    <li key={`${it.kind}-${it.card_id ?? it.session_id ?? i}`}>
                      <button
                        type="button"
                        className="dash-row"
                        onClick={onClick}
                        disabled={!onClick}
                        title={it.detail ? `${it.title}: ${it.detail}` : it.title}
                        data-kind={it.kind}
                      >
                        <span className="dash-row-main">
                          <span className="dash-row-title">{primary}</span>
                          {(sub || it.detail) && (
                            <span className="dash-row-sub">
                              {sub}
                              {sub && it.detail ? ' — ' : ''}
                              {it.detail}
                            </span>
                          )}
                        </span>
                        <span className="dash-row-time">{formatRelativeTime(it.at)}</span>
                      </button>
                    </li>
                  )
                })}
              </ul>
            </section>
          )
        })}
      </div>
    )
  }

  const urgent = items.filter((it) => it.kind === 'question' || it.kind === 'worker_error').length
  return (
    <WidgetFrame
      kind="attention"
      widgetId={widget.id}
      title={scopeName ? `Needs Attention · ${scopeName}` : 'Needs Attention'}
      statusSlot={
        data && items.length > 0 ? (
          <DashCount
            n={items.length}
            tone={urgent > 0 ? 'danger' : 'warn'}
            label={`${items.length} items`}
          />
        ) : undefined
      }
      menuItems={menuItems}
      ctx={ctx}
    >
      {body}
    </WidgetFrame>
  )
}
