import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react'
import { formatRelativeTime } from '../../../lib/review'
import WidgetFrame from '../WidgetFrame'
import type { InfoWidgetProps } from './types'
import {
  formatDuration,
  isFailure,
  sshFleetFetch,
  SshFleetError,
  type SshActivity,
  type SshActivityPage,
  type SshHost,
} from './sshFleet'
import { DashNoMatch } from './DashParts'
import {
  FilterBar,
  FilterButton,
  inSet,
  matchesSearch,
  useWidgetFilters,
  type FilterDef,
} from './filters'
import '../../../styles/dashboard-ssh.css'

const PAGE_SIZE = 50
const POLL_MS = 5_000
const FRESH_MS = 2_000
const HOUR_MS = 3_600_000

interface FeedState {
  /** Newest first. */
  items: SshActivity[]
  cursor: number
  hasMore: boolean
  loaded: boolean
}

const EMPTY: FeedState = { items: [], cursor: 0, hasMore: false, loaded: false }

/** Live feed of the `ssh_activity` log, optionally scoped to one host. The
 *  plugin returns pages oldest-first; we keep them newest-first. */
function useSshActivity(host: string) {
  const [feed, setFeed] = useState<FeedState>(EMPTY)
  const [error, setError] = useState<{ message: string; notInstalled: boolean } | null>(null)
  const [fresh, setFresh] = useState<Set<number>>(() => new Set())
  const [loadingOlder, setLoadingOlder] = useState(false)
  // Bumped on unmount so in-flight responses drop (the widget remounts per
  // host via `key`, which also resets this state).
  const gen = useRef(0)
  const feedRef = useRef(feed)
  useEffect(() => {
    feedRef.current = feed
  }, [feed])
  const polling = useRef(false)

  const q = `host=${encodeURIComponent(host)}`

  const loadFirst = useCallback(async () => {
    const mine = gen.current
    try {
      const d = await sshFleetFetch<SshActivityPage>(`/activity?${q}&since=0&limit=${PAGE_SIZE}`)
      if (mine !== gen.current) return
      setFeed({
        items: [...d.items].reverse(),
        cursor: d.cursor || 0,
        hasMore: !!d.has_more,
        loaded: true,
      })
      setError(null)
    } catch (e) {
      if (mine !== gen.current) return
      setError({
        message: e instanceof Error ? e.message : 'Failed to load',
        notInstalled: e instanceof SshFleetError && e.notInstalled,
      })
    }
  }, [q])

  const poll = useCallback(async () => {
    const cur = feedRef.current
    if (polling.current || !cur.loaded) return
    polling.current = true
    const mine = gen.current
    try {
      const d = await sshFleetFetch<SshActivityPage>(`/activity?${q}&since=${cur.cursor}`)
      if (mine !== gen.current) return
      setError(null)
      if (!d.items.length) return
      const seen = new Set(feedRef.current.items.map((a) => a.id))
      const added = d.items.filter((a) => !seen.has(a.id)).reverse()
      setFeed((f) => ({ ...f, items: [...added, ...f.items], cursor: d.cursor || f.cursor }))
      if (added.length) {
        const ids = added.map((a) => a.id)
        setFresh((s) => new Set([...s, ...ids]))
        window.setTimeout(() => {
          setFresh((s) => {
            const n = new Set(s)
            ids.forEach((id) => n.delete(id))
            return n
          })
        }, FRESH_MS)
      }
    } catch {
      // Transient; the next tick retries.
    } finally {
      polling.current = false
    }
  }, [q])

  const loadOlder = useCallback(async () => {
    const cur = feedRef.current
    const oldest = cur.items[cur.items.length - 1]
    if (!oldest) return
    const mine = gen.current
    setLoadingOlder(true)
    try {
      const d = await sshFleetFetch<SshActivityPage>(
        `/activity?${q}&before=${oldest.id}&limit=${PAGE_SIZE}`,
      )
      if (mine !== gen.current) return
      setFeed((f) => ({
        ...f,
        items: [...f.items, ...[...d.items].reverse()],
        hasMore: !!d.has_more,
      }))
    } catch (e) {
      if (mine !== gen.current) return
      setError({ message: e instanceof Error ? e.message : 'Failed to load', notInstalled: false })
    } finally {
      if (mine === gen.current) setLoadingOlder(false)
    }
  }, [q])

  useEffect(() => {
    const generation = gen
    const first = window.setTimeout(() => void loadFirst(), 0)
    const t = window.setInterval(() => {
      if (document.visibilityState !== 'visible') return
      if (feedRef.current.loaded) void poll()
      else void loadFirst()
    }, POLL_MS)
    return () => {
      generation.current++
      window.clearTimeout(first)
      window.clearInterval(t)
    }
  }, [loadFirst, poll])

  return { feed, error, fresh, loadingOlder, loadOlder, reload: loadFirst }
}

/** Label for a scoped widget's title: from the hosts list, else the feed. */
function useHostLabel(hostRef: string | null | undefined, items: SshActivity[]): string | null {
  const [label, setLabel] = useState<string | null>(null)
  useEffect(() => {
    if (!hostRef) return
    let live = true
    sshFleetFetch<{ hosts: SshHost[] }>('/hosts')
      .then((d) => {
        const h = d.hosts.find((x) => x.id === hostRef)
        if (live && h) setLabel(h.label)
      })
      .catch(() => {})
    return () => {
      live = false
    }
  }, [hostRef])
  if (!hostRef) return null
  return label ?? items.find((a) => a.host_id === hostRef)?.host_label ?? hostRef
}

function Pill({ a }: { a: SshActivity }) {
  if (isFailure(a)) {
    const text = a.exit_code != null ? `exit ${a.exit_code}` : 'error'
    return (
      <span className="ssh-pill ssh-pill-bad" title={a.error || undefined}>
        {text}
      </span>
    )
  }
  return (
    <span
      className="ssh-pill ssh-pill-ok"
      title={a.exit_code != null ? `exit ${a.exit_code}` : undefined}
    >
      ok
    </span>
  )
}

function Preview({ label, text, tone }: { label: string; text: string; tone?: 'err' | 'diff' }) {
  return (
    <div className="ssh-act-preview">
      <span className="ssh-act-preview-label">{label}</span>
      <pre className={`ssh-act-pre${tone ? ` ssh-act-pre-${tone}` : ''}`}>
        {tone === 'diff'
          ? text.split('\n').map((line, i) => (
              <span
                key={i}
                className={
                  line.startsWith('+ ')
                    ? 'ssh-diff-add'
                    : line.startsWith('- ')
                      ? 'ssh-diff-del'
                      : line.startsWith('@@')
                        ? 'ssh-diff-hunk'
                        : undefined
                }
              >
                {line + '\n'}
              </span>
            ))
          : text}
      </pre>
    </div>
  )
}

function Row({
  a,
  showHost,
  fresh,
  expanded,
  onToggle,
  onOpenSession,
}: {
  a: SshActivity
  showHost: boolean
  fresh: boolean
  expanded: boolean
  onToggle: () => void
  onOpenSession: (id: string) => void
}) {
  const failed = isFailure(a)
  const sid = a.session_id
  const hasDetail = !!(a.stdout_preview || a.stderr_preview || a.diff_preview || a.error)
  return (
    <li
      className={`ssh-act-row${fresh ? ' ssh-act-fresh' : ''}${failed ? ' ssh-act-failed' : ''}${expanded ? ' expanded' : ''}`}
      data-testid="dash-ssh-activity-row"
      data-activity-id={a.id}
      data-ok={!failed}
    >
      <div className="ssh-act-line">
        <button
          type="button"
          className="ssh-act-main"
          aria-expanded={expanded}
          onClick={onToggle}
          title={hasDetail ? 'Show output' : undefined}
        >
          <span className="ssh-act-time" title={a.ts ? new Date(a.ts).toLocaleString() : undefined}>
            {a.ts ? formatRelativeTime(a.ts) : '—'}
          </span>
          {showHost && (
            <span className="ssh-act-host" title={a.host_label}>
              {a.host_label}
            </span>
          )}
          <span className="ssh-act-tool" title={a.tool}>
            {a.tool}
          </span>
          <span className="ssh-act-text">
            <span className="ssh-act-summary" title={a.summary}>
              {a.summary}
            </span>
            {a.reason && (
              <span className="ssh-act-reason" title={a.reason}>
                {a.reason}
              </span>
            )}
          </span>
          <Pill a={a} />
          <span className="ssh-act-dur">{formatDuration(a.duration_ms)}</span>
        </button>
        {sid ? (
          <button
            type="button"
            className="ssh-act-session"
            title="Open session"
            onClick={() => onOpenSession(sid)}
          >
            {a.session_name || `Session ${sid.slice(0, 8)}`}
          </button>
        ) : (
          <span className="ssh-act-session ssh-act-session-none">
            {a.source === 'dashboard' ? 'Dashboard' : '—'}
          </span>
        )}
      </div>
      {expanded && (
        <div className="ssh-act-detail" data-testid="dash-ssh-activity-detail">
          {a.error && <Preview label="error" text={a.error} tone="err" />}
          {a.diff_preview && <Preview label="diff" text={a.diff_preview} tone="diff" />}
          {a.stdout_preview && <Preview label="stdout" text={a.stdout_preview} />}
          {a.stderr_preview && <Preview label="stderr" text={a.stderr_preview} tone="err" />}
          {!hasDetail && <p className="ssh-muted">No output captured.</p>}
        </div>
      )}
    </li>
  )
}

/** Commands agents ran over SSH (the ssh-fleet activity log): newest first,
 *  polled every 5s with the plugin's `since` cursor. `hostRef` scopes it;
 *  changing it remounts the feed so no state leaks across hosts. */
export default function SshActivityWidget(props: InfoWidgetProps) {
  return <SshActivityFeed key={props.widget.hostRef || 'all'} {...props} />
}

function SshActivityFeed(props: InfoWidgetProps) {
  const { widget, ctx, menuItems, onOpenSession } = props
  const hostRef = widget.hostRef || null
  const { feed, error, fresh, loadingOlder, loadOlder, reload } = useSshActivity(hostRef ?? 'all')
  const hostLabel = useHostLabel(hostRef, feed.items)
  const [expanded, setExpanded] = useState<number | null>(null)
  // Keep relative times and the last-hour count honest between polls.
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now()), 30_000)
    return () => window.clearInterval(t)
  }, [])

  const failures = feed.items.filter((a) => {
    if (!isFailure(a) || !a.ts) return false
    const t = new Date(a.ts).getTime()
    return !Number.isNaN(t) && now - t < HOUR_MS
  }).length

  const { filters, set, clear, activeCount, open, toggle } = useWidgetFilters(props)
  const tools = [...new Set(feed.items.map((a) => a.tool))].sort()
  const sessions = new Map<string, string>()
  for (const a of feed.items)
    if (a.session_id) sessions.set(a.session_id, a.session_name || 'Session')
  const defs: FilterDef[] = [
    { key: 'q', kind: 'search', placeholder: 'Search commands…' },
    {
      key: 'tool',
      kind: 'chips',
      label: 'Tool',
      options: tools.map((t) => ({ value: t, label: t.replace(/^ssh_/, '') })),
    },
    { key: 'failed', kind: 'toggle', label: 'Failed only' },
    {
      key: 'session',
      kind: 'combo',
      label: 'Session',
      options: [...sessions].map(([value, label]) => ({ value, label })),
    },
  ]
  const shown = feed.items.filter(
    (a) =>
      inSet(filters.tool, a.tool) &&
      (filters.failed !== true || isFailure(a)) &&
      inSet(filters.session, a.session_id) &&
      matchesSearch(filters.q, a.summary, a.reason, a.session_name),
  )

  let body: ReactNode
  if (error?.notInstalled) {
    body = (
      <div className="ssh-empty" data-testid="dash-ssh_activity-empty" data-reason="not-installed">
        SSH Fleet plugin not installed
      </div>
    )
  } else if (!feed.loaded) {
    body = error ? (
      <div className="ssh-empty" role="alert">
        <p>{error.message}</p>
        <button type="button" className="btn-secondary btn-sm" onClick={() => void reload()}>
          Retry
        </button>
      </div>
    ) : (
      <div className="ssh-empty ssh-muted" aria-busy="true">
        Loading…
      </div>
    )
  } else if (feed.items.length === 0) {
    body = (
      <div className="ssh-empty" data-testid="dash-ssh_activity-empty">
        No commands run yet
      </div>
    )
  } else if (shown.length === 0) {
    body = <DashNoMatch onClear={clear} />
  } else {
    body = (
      <div className="ssh-scroll">
        <ul className="ssh-act-list" data-testid="dash-ssh_activity-list">
          {shown.map((a) => (
            <Row
              key={a.id}
              a={a}
              showHost={!hostRef}
              fresh={fresh.has(a.id)}
              expanded={expanded === a.id}
              onToggle={() => setExpanded((cur) => (cur === a.id ? null : a.id))}
              onOpenSession={onOpenSession}
            />
          ))}
        </ul>
        {feed.hasMore && (
          <button
            type="button"
            className="ssh-load-older"
            disabled={loadingOlder}
            onClick={() => void loadOlder()}
          >
            {loadingOlder ? 'Loading…' : 'Load older'}
          </button>
        )}
      </div>
    )
  }

  return (
    <WidgetFrame
      kind="ssh_activity"
      widgetId={widget.id}
      title={hostLabel ? `SSH · ${hostLabel}` : 'SSH Activity'}
      statusSlot={
        feed.loaded && feed.items.length > 0 ? (
          <span
            className={`ssh-status-chip${failures > 0 ? ' ssh-status-bad' : ''}`}
            title="Failed commands in the last hour"
            data-testid="dash-ssh-activity-failures"
          >
            {failures} failed/1h
          </span>
        ) : undefined
      }
      actions={<FilterButton activeCount={activeCount} open={open} onToggle={toggle} />}
      menuItems={menuItems}
      ctx={ctx}
      dataAttrs={{ 'data-host-ref': hostRef ?? undefined }}
    >
      {open && <FilterBar defs={defs} filters={filters} set={set} clear={clear} />}
      {body}
    </WidgetFrame>
  )
}
