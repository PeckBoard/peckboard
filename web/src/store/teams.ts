import { authedFetch } from './auth'

/**
 * Teams of Peckboard servers (`/api/teams*`, admin only). A member is a
 * server, identified by its key; nobody has an account anywhere. Plain API
 * calls only: the Teams settings page owns its state and reloads on the
 * `team-update` WebSocket event (relayed as `peckboard:team-update`).
 *
 * Not to be confused with a skills "team source" (a shared git repo of
 * skills, `store/skills.ts`).
 */

export type TeamRole = 'owner' | 'admin' | 'member'
export type TeamState = 'member' | 'pending' | 'left' | 'removed'

export const ROLE_LABEL: Record<TeamRole, string> = {
  owner: 'Owner',
  admin: 'Admin',
  member: 'Member',
}

export interface TeamMember {
  key: string
  fingerprint: string
  label: string
  role: TeamRole
  added_at: string
  is_self: boolean
}

export interface Team {
  id: string
  name: string
  state: TeamState
  role: TeamRole | null
  members: TeamMember[]
  /** While a join request is pending: the code to read to the inviter. */
  pending_code: string | null
  /** Why the last check with the team service failed, if it did. */
  last_error: string | null
  updated_at: string
}

export interface TeamsOverview {
  server_key: string
  server_fingerprint: string
  default_label: string
  hub_url: string
  teams: Team[]
}

export interface TeamInvite {
  invite_id: string
  link: string
  expires_at: string
  created_at: string
}

export interface JoinRequest {
  request_id: string
  label: string
  key: string
  fingerprint: string
  created_at: string
  /** False: the request used an invite another server made. */
  can_approve: boolean
  /** False (with can_approve): not from someone holding the link. */
  genuine: boolean
  /** The verification code, e.g. "123 456". */
  code: string | null
}

export interface InvitePreview {
  team_id: string
  name: string
  members: number
}

/** An API error with the server's plain-words message and its code. */
export class TeamApiError extends Error {
  code: string | null
  status: number
  constructor(message: string, code: string | null, status: number) {
    super(message)
    this.name = 'TeamApiError'
    this.code = code
    this.status = status
  }
}

async function readJson<T>(res: Response): Promise<T> {
  if (res.ok) {
    if (res.status === 204) return undefined as T
    const text = await res.text()
    return (text ? JSON.parse(text) : undefined) as T
  }
  const body = (await res.json().catch(() => null)) as { error?: string; code?: string } | null
  throw new TeamApiError(
    body?.error || `Request failed (${res.status})`,
    body?.code ?? null,
    res.status,
  )
}

function send<T>(url: string, method: string, data?: unknown): Promise<T> {
  return authedFetch(url, {
    method,
    headers: data === undefined ? undefined : { 'Content-Type': 'application/json' },
    body: data === undefined ? undefined : JSON.stringify(data),
  }).then((r) => readJson<T>(r))
}

const team = (id: string) => `/api/teams/${encodeURIComponent(id)}`

export const getTeams = () => send<TeamsOverview>('/api/teams', 'GET')
export const refreshTeams = () => send<{ changed: number }>('/api/teams/refresh', 'POST')
export const createTeam = (name: string, label: string) =>
  send<Team>('/api/teams', 'POST', { name, label })
export const previewInvite = (link: string) =>
  send<InvitePreview>('/api/teams/join/preview', 'POST', { link })
export const joinTeam = (link: string, label: string) =>
  send<Team>('/api/teams/join', 'POST', { link, label })
export const refreshTeam = (id: string) => send<Team>(`${team(id)}/refresh`, 'POST')
export const forgetTeam = (id: string) => send<void>(team(id), 'DELETE')
export const renameTeam = (id: string, name: string) =>
  send<Team>(`${team(id)}/rename`, 'POST', { name })
export const leaveTeam = (id: string) => send<Team>(`${team(id)}/leave`, 'POST')
export const setMemberRole = (id: string, key: string, role: TeamRole) =>
  send<Team>(`${team(id)}/members/role`, 'POST', { key, role })
export const removeMember = (id: string, key: string) =>
  send<Team>(`${team(id)}/members/remove`, 'POST', { key })
export const listInvites = (id: string) =>
  send<{ invites: TeamInvite[] }>(`${team(id)}/invites`, 'GET').then((b) => b?.invites ?? [])
export const createInvite = (id: string, days?: number) =>
  send<TeamInvite>(`${team(id)}/invites`, 'POST', days ? { days } : {})
export const withdrawInvite = (id: string, inviteId: string) =>
  send<void>(`${team(id)}/invites/${encodeURIComponent(inviteId)}`, 'DELETE')
export const listRequests = (id: string) =>
  send<{ requests: JoinRequest[] }>(`${team(id)}/requests`, 'GET').then((b) => b?.requests ?? [])
export const approveRequest = (id: string, requestId: string) =>
  send<Team>(`${team(id)}/requests/${encodeURIComponent(requestId)}/approve`, 'POST')
export const rejectRequest = (id: string, requestId: string) =>
  send<void>(`${team(id)}/requests/${encodeURIComponent(requestId)}/reject`, 'POST')

/** Who may do what, per the team's role rules. */
export const canManage = (role: TeamRole | null) => role === 'owner' || role === 'admin'

/** May a server with role `me` remove `target`? Owners remove anyone else;
 *  admins only plain members. Nobody removes themself (that is Leave). */
export function canRemove(me: TeamRole | null, target: TeamMember): boolean {
  if (target.is_self) return false
  if (me === 'owner') return true
  return me === 'admin' && target.role === 'member'
}

/** Only owners change roles. An owner changes its own role only while
 *  another owner remains (a team always keeps one). */
export function canChangeRole(me: TeamRole | null, target: TeamMember, owners: number): boolean {
  if (me !== 'owner') return false
  return !target.is_self || owners > 1
}

/** A name for a team or server: 1 to 64 characters after trimming. */
export function nameError(value: string, what: string): string {
  const v = value.trim()
  if (!v) return `Enter ${what}`
  if ([...v].length > 64) return `Keep ${what} to 64 characters or fewer`
  return ''
}

/** A server timestamp in the viewer's locale (falls back to the raw text). */
export function formatWhen(at: string): string {
  const d = new Date(at)
  return isNaN(d.getTime()) ? at : d.toLocaleString()
}
