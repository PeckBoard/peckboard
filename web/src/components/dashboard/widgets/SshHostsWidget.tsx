import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react'
import { formatRelativeTime } from '../../../lib/review'
import WidgetFrame from '../WidgetFrame'
import type { InfoWidgetProps } from './types'
import { sshFleetFetch, SshFleetError, type SshHost } from './sshFleet'
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

const POLL_MS = 30_000

function statusTone(s: string): 'ok' | 'error' | 'unknown' {
  return s === 'ok' ? 'ok' : s === 'error' ? 'error' : 'unknown'
}

function useSshHosts() {
  const [hosts, setHosts] = useState<SshHost[] | null>(null)
  const [error, setError] = useState<{ message: string; notInstalled: boolean } | null>(null)
  const seq = useRef(0)

  const load = useCallback(async () => {
    const mine = ++seq.current
    try {
      const d = await sshFleetFetch<{ hosts: SshHost[] }>('/hosts')
      if (mine !== seq.current) return
      setHosts(d.hosts)
      setError(null)
    } catch (e) {
      if (mine !== seq.current) return
      setError({
        message: e instanceof Error ? e.message : 'Failed to load',
        notInstalled: e instanceof SshFleetError && e.notInstalled,
      })
    }
  }, [])

  useEffect(() => {
    const first = window.setTimeout(() => void load(), 0)
    const t = window.setInterval(() => {
      if (document.visibilityState === 'visible') void load()
    }, POLL_MS)
    return () => {
      window.clearTimeout(first)
      window.clearInterval(t)
    }
  }, [load])

  return { hosts, error, reload: load }
}

/** The ssh-fleet host registry at a glance: reachability, last seen, a
 *  Probe (connect + auth, no command) and an Open terminal shortcut. */
export default function SshHostsWidget(props: InfoWidgetProps) {
  const { widget, ctx, menuItems, onOpenTerminal } = props
  const { hosts, error, reload } = useSshHosts()
  const [probing, setProbing] = useState<Set<string>>(() => new Set())
  const [probeErr, setProbeErr] = useState<Record<string, string>>({})
  const [, tick] = useState(0)
  useEffect(() => {
    const t = window.setInterval(() => tick((n) => n + 1), 60_000)
    return () => window.clearInterval(t)
  }, [])
  const { filters, set, clear, activeCount, open, toggle } = useWidgetFilters(props)
  const tags = [...new Set((hosts ?? []).flatMap((h) => h.tags))].sort()
  const defs: FilterDef[] = [
    { key: 'q', kind: 'search', placeholder: 'Search hosts…' },
    {
      key: 'status',
      kind: 'chips',
      label: 'Status',
      options: [
        { value: 'ok', label: 'OK' },
        { value: 'error', label: 'Error' },
        { value: 'unknown', label: 'Unknown' },
      ],
    },
    { key: 'tag', kind: 'combo', label: 'Tag', options: tags.map((t) => ({ value: t, label: t })) },
  ]
  const tagSel = Array.isArray(filters.tag) ? filters.tag : []
  const shown = (hosts ?? []).filter(
    (h) =>
      inSet(filters.status, statusTone(h.last_status)) &&
      (tagSel.length === 0 || h.tags.some((t) => tagSel.includes(t))) &&
      matchesSearch(filters.q, h.label, h.hostname, h.username),
  )

  const probe = async (id: string) => {
    setProbing((s) => new Set(s).add(id))
    setProbeErr((m) => {
      const n = { ...m }
      delete n[id]
      return n
    })
    try {
      await sshFleetFetch('/probe', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ host: id }),
      })
    } catch (e) {
      setProbeErr((m) => ({ ...m, [id]: e instanceof Error ? e.message : 'Probe failed' }))
    } finally {
      setProbing((s) => {
        const n = new Set(s)
        n.delete(id)
        return n
      })
      void reload()
    }
  }

  let body: ReactNode
  if (error?.notInstalled) {
    body = (
      <div className="ssh-empty" data-testid="dash-ssh_hosts-empty" data-reason="not-installed">
        SSH Fleet plugin not installed
      </div>
    )
  } else if (!hosts) {
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
  } else if (hosts.length === 0) {
    body = (
      <div className="ssh-empty" data-testid="dash-ssh_hosts-empty">
        No SSH hosts yet — add one on the SSH Fleet page
      </div>
    )
  } else if (shown.length === 0) {
    body = <DashNoMatch onClear={clear} />
  } else {
    body = (
      <div className="ssh-scroll">
        <ul className="ssh-host-list" data-testid="dash-ssh_hosts-list">
          {shown.map((h) => {
            const tone = statusTone(h.last_status)
            const busy = probing.has(h.id)
            const err = probeErr[h.id] ?? (tone === 'error' ? h.last_error : null)
            return (
              <li
                key={h.id}
                className="ssh-host-row"
                data-testid="dash-ssh-host-row"
                data-host-id={h.id}
                data-status={tone}
              >
                <span
                  className={`ssh-dot ssh-dot-${tone}`}
                  role="img"
                  aria-label={`Status: ${tone}`}
                  title={h.last_error || tone}
                />
                <div className="ssh-host-main">
                  <div className="ssh-host-line">
                    <span className="ssh-host-label" title={h.label}>
                      {h.label}
                    </span>
                    <span className="ssh-host-addr" title={`${h.username}@${h.hostname}:${h.port}`}>
                      {h.username}@{h.hostname}:{h.port}
                    </span>
                    {h.tags.map((t) => (
                      <span key={t} className="ssh-tag">
                        {t}
                      </span>
                    ))}
                  </div>
                  {err && (
                    <div className="ssh-host-error" title={err}>
                      {err}
                    </div>
                  )}
                </div>
                <span
                  className="ssh-host-seen"
                  title={h.last_seen ? new Date(h.last_seen).toLocaleString() : 'Never connected'}
                >
                  {h.last_seen ? formatRelativeTime(h.last_seen) : 'never'}
                </span>
                <div className="ssh-host-actions">
                  <button
                    type="button"
                    className="ssh-btn"
                    disabled={busy}
                    aria-busy={busy || undefined}
                    title="Re-check connectivity (connect + auth, no command)"
                    data-testid="dash-ssh-host-probe"
                    onClick={() => void probe(h.id)}
                  >
                    {busy ? <span className="ssh-spinner" aria-label="Probing" /> : 'Probe'}
                  </button>
                  {onOpenTerminal && (
                    <button
                      type="button"
                      className="ssh-btn"
                      title="Open a terminal on this host"
                      aria-label={`Open terminal on ${h.label}`}
                      data-testid="dash-ssh-host-terminal"
                      onClick={() => onOpenTerminal('ssh-fleet', h.id)}
                    >
                      Terminal
                    </button>
                  )}
                </div>
              </li>
            )
          })}
        </ul>
      </div>
    )
  }

  const bad = hosts?.filter((h) => statusTone(h.last_status) === 'error').length ?? 0
  const ok = hosts?.filter((h) => statusTone(h.last_status) === 'ok').length ?? 0

  return (
    <WidgetFrame
      kind="ssh_hosts"
      widgetId={widget.id}
      title="SSH Hosts"
      statusSlot={
        hosts && hosts.length > 0 ? (
          <span
            className={`ssh-status-chip${bad > 0 ? ' ssh-status-bad' : ''}`}
            title={`${ok} ok · ${bad} failing · ${hosts.length - ok - bad} unknown`}
          >
            {ok}/{hosts.length} up
          </span>
        ) : undefined
      }
      actions={<FilterButton activeCount={activeCount} open={open} onToggle={toggle} />}
      menuItems={menuItems}
      ctx={ctx}
    >
      {open && <FilterBar defs={defs} filters={filters} set={set} clear={clear} />}
      {body}
    </WidgetFrame>
  )
}
