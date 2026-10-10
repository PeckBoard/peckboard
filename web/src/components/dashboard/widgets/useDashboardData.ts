import { useCallback, useEffect, useRef, useState } from 'react'
import { authedFetch } from '../../../store/auth'

const POLL_MS = 30_000
/** Coalesces bursts of card events (a worker moving several cards). */
const REFRESH_DEBOUNCE_MS = 400

/** WS broadcasts (store/ws.ts) that can change dashboard read models. */
export type DashboardEvent =
  | 'card-update'
  | 'card-delete'
  | 'project-update'
  | 'worker-question'
  | 'session-updated'

/** Project a WS event concerns, or undefined when it doesn't say. */
function eventProjectId(e: Event): string | undefined {
  const d = (e as CustomEvent).detail?.data
  return d?.card?.project_id ?? d?.project?.id ?? d?.projectId ?? d?.project_id ?? undefined
}

/**
 * Fetches a `/api/dashboard/*` read model — the ProjectWidget pattern:
 * debounced refetch on relevant `peckboard:*` events, a 30s visible-only
 * poll, and a seq ref that drops stale responses. `path` may carry its own
 * query string; `?project_id=` is appended when `scopeProjectId` is set.
 * Pass `path: null` to skip fetching (e.g. a required scope is missing).
 */
export function useDashboardData<T>(
  path: string | null,
  scopeProjectId: string | null,
  events: DashboardEvent[],
) {
  // Results are tagged with the URL they answer, so a scope / range change
  // shows the skeleton instead of the previous URL's data.
  const [state, setState] = useState<{ url: string; data: T | null; error: string | null } | null>(
    null,
  )
  const seq = useRef(0)

  const url =
    path === null
      ? null
      : scopeProjectId
        ? `${path}${path.includes('?') ? '&' : '?'}project_id=${encodeURIComponent(scopeProjectId)}`
        : path

  const load = useCallback(async () => {
    if (url === null) return
    const mine = ++seq.current
    try {
      const res = await authedFetch(url)
      if (!res.ok) {
        const body = await res.json().catch(() => null)
        throw new Error((body && typeof body.error === 'string' && body.error) || 'Failed to load')
      }
      const json = (await res.json()) as T
      if (mine !== seq.current) return
      if (mine !== seq.current) return
      setState({ url, data: json, error: null })
    } catch (e) {
      if (mine !== seq.current) return
      const error = e instanceof Error ? e.message : 'Failed to load'
      setState((s) => ({ url, data: s?.url === url ? s.data : null, error }))
    }
  }, [url])

  const eventsKey = events.join(',')
  useEffect(() => {
    if (url === null) return
    let timer: number | null = null
    const soon = (delay = REFRESH_DEBOUNCE_MS) => {
      if (timer !== null) window.clearTimeout(timer)
      timer = window.setTimeout(() => {
        timer = null
        void load()
      }, delay)
    }
    soon(0)
    const onEvent = (e: Event) => {
      if (scopeProjectId) {
        const pid = eventProjectId(e)
        if (pid !== undefined && pid !== scopeProjectId) return
      }
      soon()
    }
    const names = eventsKey ? eventsKey.split(',').map((n) => `peckboard:${n}`) : []
    for (const n of names) window.addEventListener(n, onEvent)
    const poll = window.setInterval(() => {
      if (document.visibilityState === 'visible') void load()
    }, POLL_MS)
    return () => {
      for (const n of names) window.removeEventListener(n, onEvent)
      window.clearInterval(poll)
      if (timer !== null) window.clearTimeout(timer)
    }
  }, [url, load, eventsKey, scopeProjectId])

  const current = state && state.url === url ? state : null
  return { data: current?.data ?? null, error: current?.error ?? null, reload: load }
}

/** `in_review` → "In review". */
export function humanize(s: string): string {
  const t = s.replace(/[_-]+/g, ' ').trim()
  return t ? t[0].toUpperCase() + t.slice(1) : s
}

/** Compact "42s" / "3m 05s" / "2h 04m" / "1d 3h" duration. */
export function fmtElapsed(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000))
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m ${String(s % 60).padStart(2, '0')}s`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h}h ${String(m % 60).padStart(2, '0')}m`
  return `${Math.floor(h / 24)}d ${h % 24}h`
}
