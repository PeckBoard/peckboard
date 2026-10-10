import { authedFetch } from '../../../store/auth'

/** The ssh-fleet plugin's app-UI endpoints, via core's authed proxy. */
export const SSH_FLEET_API = '/api/plugin-ui/ssh-fleet'

/** One ssh_* tool call (`peck-plugins/ssh-fleet/src/activity.ts`). */
export interface SshActivity {
  id: number
  ts: string | null
  host_id: string | null
  host_label: string
  tool: string
  summary: string
  ok: boolean
  exit_code?: number | null
  duration_ms?: number | null
  bytes?: number | null
  stdout_preview?: string
  stderr_preview?: string
  error?: string
  diff_preview?: string
  session_id?: string | null
  session_name?: string | null
  reason?: string | null
  source?: 'agent' | 'dashboard'
}

export interface SshActivityPage {
  items: SshActivity[]
  cursor: number
  has_more: boolean
}

/** Secret-free host view (`PublicHost` in `hosts.ts`). */
export interface SshHost {
  id: string
  label: string
  hostname: string
  port: number
  username: string
  tags: string[]
  last_status: string
  last_seen: string | null
  last_error: string | null
}

/** Thrown for non-2xx; `notInstalled` when the proxy has no ssh-fleet route. */
export class SshFleetError extends Error {
  notInstalled: boolean
  constructor(message: string, notInstalled: boolean) {
    super(message)
    this.notInstalled = notInstalled
  }
}

export async function sshFleetFetch<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await authedFetch(`${SSH_FLEET_API}${path}`, init)
  if (!res.ok) {
    const body = await res.json().catch(() => null)
    const msg = (body && typeof body.error === 'string' && body.error) || `HTTP ${res.status}`
    throw new SshFleetError(msg, res.status === 404)
  }
  return (await res.json()) as T
}

/** "840ms", "2.4s", "3m 05s". */
export function formatDuration(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms)) return ''
  if (ms < 1000) return `${Math.round(ms)}ms`
  if (ms < 60_000) return `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)}s`
  const s = Math.round(ms / 1000)
  return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, '0')}s`
}

export function isFailure(a: SshActivity): boolean {
  return !a.ok || !!a.error
}
