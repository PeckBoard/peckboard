import { useEffect, useMemo, useState, type MouseEvent, type ReactNode } from 'react'
import { authedFetch } from '../store/auth'
import { useSessionsStore } from '../store/sessions'
import { useProjectsStore } from '../store/projects'
import { useTabsStore } from '../store/tabs'
import { openReport } from '../lib/reports'
import { parseReportTabId } from './tabKinds'
import {
  buildDisplayItems,
  deriveAgentStatus,
  getStatusDotClass,
  getStatusLabel,
  type AgentStatus,
} from './chat/events'
import type { Card, Event, Project, Session } from '../types/api'
import type { LinkPreviewData } from '../store/linkPreviews'

type Internal = NonNullable<LinkPreviewData['internal']>

/** GET `url` as JSON; `undefined` while loading, `null` when it failed
 *  (403 / 404 / network) — the caller then renders nothing extra. */
function useJson<T>(url: string | null): T | null | undefined {
  const [state, setState] = useState<{ url: string | null; data: T | null | undefined }>({
    url,
    data: undefined,
  })
  useEffect(() => {
    if (!url) return
    let cancelled = false
    authedFetch(url)
      .then((r) => (r.ok ? (r.json() as Promise<T>) : null))
      .catch(() => null)
      .then((data) => !cancelled && setState({ url, data }))
    return () => {
      cancelled = true
    }
  }, [url])
  return state.url === url ? state.data : undefined
}

/** Open an in-app route the way [[SubagentTranscript]] does: pushState +
 *  a synthetic popstate so App's router follows. */
function go(path: string) {
  if (window.location.pathname + window.location.search !== path) {
    window.history.pushState(null, '', path)
  }
  window.dispatchEvent(new PopStateEvent('popstate'))
}

/** Plain left click stays in the SPA; modified clicks keep the browser's
 *  open-in-new-tab behaviour via the real href. */
function inApp(open: () => void) {
  return (e: MouseEvent<HTMLAnchorElement>) => {
    if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return
    e.preventDefault()
    open()
  }
}

function Shell({
  href,
  entity,
  icon,
  title,
  sub,
  children,
  onOpen,
}: {
  href: string
  entity: Internal['entity']
  icon: string
  title: string
  sub?: ReactNode
  children?: ReactNode
  onOpen: () => void
}) {
  return (
    <a
      className="link-card link-internal"
      href={href}
      onClick={inApp(onOpen)}
      data-testid="link-internal"
      data-entity={entity}
    >
      <span className="link-card-body">
        <span className="link-card-site">
          <span className="link-internal-icon" aria-hidden="true">
            {icon}
          </span>
          <span className="link-card-sitename">Peckboard</span>
          {sub && <span className="link-card-domain">{sub}</span>}
        </span>
        <span className="link-card-title" data-testid="link-internal-title">
          {title}
        </span>
        {children}
      </span>
    </a>
  )
}

function lastMessage(events: Event[]): string | null {
  const items = buildDisplayItems(events)
  for (let i = items.length - 1; i >= 0; i--) {
    const it = items[i]
    if ((it.type === 'assistant' || it.type === 'user') && it.text.trim()) return it.text.trim()
  }
  return null
}

function SessionCard({ id, href }: { id: string; href: string }) {
  const liveRow = useSessionsStore((s) => s.sessions.find((x) => x.id === id))
  const liveEvents = useSessionsStore((s) => s.eventsBySession[id])
  const processing = useSessionsStore((s) => s.processing.has(id))
  const fetched = useJson<Session>(`/api/sessions/${encodeURIComponent(id)}`)
  // History only when the session isn't already loaded (its chat is open).
  const history = useJson<Event[]>(
    liveEvents ? null : `/api/sessions/${encodeURIComponent(id)}/events?limit=40`,
  )
  const events = liveEvents ?? history ?? null
  const status: AgentStatus = useMemo(() => {
    const derived = events ? deriveAgentStatus(events) : 'idle'
    return processing && derived === 'idle' ? 'working' : derived
  }, [events, processing])
  const snippet = useMemo(() => (events ? lastMessage(events) : null), [events])
  const session = liveRow ?? fetched
  if (fetched === null || !session) return null
  return (
    <Shell
      href={href}
      entity="session"
      icon={'\u{1F4AC}'}
      title={session.name}
      sub="Session"
      onOpen={() => {
        go(`/sessions/${id}`)
        void useTabsStore.getState().openTab('session', id)
      }}
    >
      <span className="link-internal-meta">
        <span className={getStatusDotClass(status)} aria-hidden="true" />
        <span data-testid="link-internal-status">{getStatusLabel(status)}</span>
        {session.model && <span className="link-internal-model">{session.model}</span>}
      </span>
      {snippet && <span className="link-card-desc">{snippet}</span>}
    </Shell>
  )
}

function countCards(cards: Card[]): string {
  const done = cards.filter((c) => c.step === 'done').length
  const open = cards.length - done
  return `${cards.length} card${cards.length === 1 ? '' : 's'} · ${open} open · ${done} done`
}

function ProjectCard({ id, href }: { id: string; href: string }) {
  const liveRow = useProjectsStore((s) => s.projects.find((p) => p.id === id))
  const fetched = useJson<Project>(`/api/projects/${encodeURIComponent(id)}`)
  const cards = useJson<Card[]>(`/api/projects/${encodeURIComponent(id)}/cards`)
  const project = liveRow ?? fetched
  if (fetched === null || !project) return null
  return (
    <Shell
      href={href}
      entity="project"
      icon={'\u{1F4CB}'}
      title={project.name}
      sub="Project"
      onOpen={() => go(`/projects/${id}`)}
    >
      <span className="link-internal-meta">
        {project.status && project.status !== 'active' && (
          <span className="link-badge">{project.status}</span>
        )}
        {cards && <span>{countCards(cards)}</span>}
      </span>
    </Shell>
  )
}

function stepLabel(step: string): string {
  return step.replace(/[_-]+/g, ' ').replace(/^\w/, (c) => c.toUpperCase())
}

function CardCard({ id, projectId, href }: { id: string; projectId: string; href: string }) {
  // Live from the board when that project's cards are loaded.
  const liveCard = useProjectsStore((s) =>
    s.cardsLoadedProjectId === projectId ? s.cards.find((c) => c.id === id) : undefined,
  )
  const project = useProjectsStore((s) => s.projects.find((p) => p.id === projectId))
  const cards = useJson<Card[]>(`/api/projects/${encodeURIComponent(projectId)}/cards`)
  const card = liveCard ?? cards?.find((c) => c.id === id)
  if (cards === null || (cards && !card) || !card) return null
  return (
    <Shell
      href={href}
      entity="card"
      icon={'\u{1F5C2}\u{FE0F}'}
      title={card.title}
      sub={project ? project.name : 'Card'}
      onOpen={() => go(`/projects/${projectId}?card=${encodeURIComponent(id)}`)}
    >
      <span className="link-internal-meta">
        <span className="link-badge" data-testid="link-internal-step">
          {stepLabel(card.step)}
        </span>
        {card.blocked && <span className="link-badge link-badge-danger">Blocked</span>}
      </span>
    </Shell>
  )
}

function ViewCard({ id, href }: { id: string; href: string }) {
  const view = useJson<{ name?: string }>(`/api/me/views/${encodeURIComponent(id)}`)
  if (!view) return null
  return (
    <Shell
      href={href}
      entity="view"
      icon={'\u{1F5BC}\u{FE0F}'}
      title={view.name ?? 'View'}
      sub="View"
      onOpen={() => go(`/views/${id}`)}
    />
  )
}

function ReportCard({ id, href }: { id: string; href: string }) {
  const parsed = parseReportTabId(id)
  if (!parsed) return null
  let file = parsed.file
  try {
    file = decodeURIComponent(file)
  } catch {
    // Keep the raw segment.
  }
  return (
    <Shell
      href={href}
      entity="report"
      icon={'\u{1F4D1}'}
      title={file}
      sub="Report"
      onOpen={() => openReport(parsed.folder, parsed.file)}
    />
  )
}

/** A live card for a link to this Peckboard instance, loaded with the
 *  viewer's own permissions; nothing when they can't see the target. */
export default function InternalLinkPreview({ preview }: { preview: LinkPreviewData }) {
  const link = preview.internal
  if (!link?.id) return null
  // Same-origin path, so a click stays in this tab's SPA.
  let href = preview.url
  try {
    const u = new URL(preview.url)
    href = u.pathname + u.search
  } catch {
    // Keep the absolute URL.
  }
  switch (link.entity) {
    case 'session':
      return <SessionCard id={link.id} href={href} />
    case 'project':
      return <ProjectCard id={link.id} href={href} />
    case 'card':
      return link.project_id ? (
        <CardCard id={link.id} projectId={link.project_id} href={href} />
      ) : null
    case 'view':
      return <ViewCard id={link.id} href={href} />
    case 'report':
      return <ReportCard id={link.id} href={href} />
    default:
      return null
  }
}
