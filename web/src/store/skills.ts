import { create } from 'zustand'
import { authedFetch } from './auth'

/**
 * Skills — reusable instructions an agent can load (`SKILL.md`: frontmatter
 * + markdown body). Three scopes: `local` (this server only), `team` (a
 * private git repo source) and `public` (the Peckboard index). Shapes
 * follow the shared skills contract (`/api/skills`, `/api/skill-groups`,
 * `/api/skill-sources`).
 */

export type SkillScope = 'local' | 'team' | 'public'
export const SCOPE_LABEL: Record<SkillScope, string> = {
  local: 'Local',
  team: 'Team',
  public: 'Public',
}

export interface Skill {
  id: string
  scope: SkillScope
  source_id: string | null
  name: string
  description: string
  tags: string[]
  groups: string[]
  version: string
  body?: string
  versions: string[]
  subscribed: boolean
  /** null = follow the latest version. */
  pinned_version: string | null
  updated_at?: string
}

/** A browse-catalog row: a skill without its body, with every version the
 *  source publishes. `id` is null until the skill is installed. */
export interface CatalogSkill extends Omit<Skill, 'body' | 'id'> {
  id: string | null
  available_versions: string[]
  installed?: boolean
}

export interface SkillGroup {
  id: string
  slug: string
  name: string
  description: string
  scope: SkillScope
  source_id: string | null
  skills: string[]
}

export interface SkillSource {
  id: string
  kind: 'public' | 'team'
  name: string
  url: string
  branch: string | null
  last_sync_at: string | null
  last_error: string | null
}

export interface TeamMemoryNote {
  slug: string
  title: string
  body: string
  updated?: string | null
}

export interface GuardFinding {
  line: number
  kind: string
  excerpt: string
}

export interface SessionSkills {
  skills: string[]
  groups: string[]
  /** Each with the version the session gets (`active_version`) and the
   *  group that brought it in, if any. */
  resolved: (Omit<Skill, 'body'> & { active_version?: string; via_group?: string | null })[]
}

export type VersionBump = 'patch' | 'minor' | 'major'

/** The proprietary-content guard rejected a write (HTTP 422). Carries the
 *  offending lines so the form can show them next to the field. */
export class GuardError extends Error {
  findings: GuardFinding[]
  constructor(findings: GuardFinding[]) {
    super('The text looks like it contains proprietary content.')
    this.name = 'GuardError'
    this.findings = findings
  }
}

async function readJson<T>(res: Response): Promise<T> {
  if (res.ok) {
    if (res.status === 204) return undefined as T
    const text = await res.text()
    return (text ? JSON.parse(text) : undefined) as T
  }
  const body = (await res.json().catch(() => null)) as {
    error?: string
    findings?: GuardFinding[]
  } | null
  if (res.status === 422 && Array.isArray(body?.findings)) throw new GuardError(body.findings)
  throw new Error(body?.error || `Request failed (${res.status})`)
}

/** List endpoints may answer with a bare array or `{<key>: [...]}`. */
function listOf<T>(body: unknown, key: string): T[] {
  if (Array.isArray(body)) return body as T[]
  const v = (body as Record<string, unknown> | null)?.[key]
  return Array.isArray(v) ? (v as T[]) : []
}

function send(url: string, method: string, data?: unknown): Promise<Response> {
  return authedFetch(url, {
    method,
    headers: data === undefined ? undefined : { 'Content-Type': 'application/json' },
    body: data === undefined ? undefined : JSON.stringify(data),
  })
}

function qs(params: Record<string, string | undefined>): string {
  const p = new URLSearchParams()
  for (const [k, v] of Object.entries(params)) if (v) p.set(k, v)
  const s = p.toString()
  return s ? `?${s}` : ''
}

// ── Plain API calls (no cached state) ────────────────────────────────

export interface CatalogResult {
  items: CatalogSkill[]
  /** Sources whose index could not be read this time (shown, not fatal). */
  errors: { source_id: string; error: string }[]
}

export async function searchCatalog(q: string, sourceId?: string): Promise<CatalogResult> {
  const res = await authedFetch(`/api/skills/search${qs({ q, source_id: sourceId })}`)
  const body = await readJson<{ errors?: CatalogResult['errors'] }>(res)
  return {
    items: listOf<CatalogSkill>(body, 'items').map((s) => ({
      ...s,
      tags: s.tags ?? [],
      groups: s.groups ?? [],
      available_versions: s.available_versions ?? s.versions ?? [],
    })),
    errors: body?.errors ?? [],
  }
}

export async function getSkill(id: string): Promise<Skill> {
  return readJson<Skill>(await authedFetch(`/api/skills/${encodeURIComponent(id)}`))
}

/** Body of one version of a skill. */
export async function getSkillVersion(id: string, version: string): Promise<string> {
  const res = await authedFetch(
    `/api/skills/${encodeURIComponent(id)}/versions/${encodeURIComponent(version)}`,
  )
  const body = await readJson<{ body?: string } | string>(res)
  return typeof body === 'string' ? body : (body?.body ?? '')
}

export async function listTeamMemory(sourceId: string): Promise<TeamMemoryNote[]> {
  const res = await authedFetch(`/api/skill-sources/${encodeURIComponent(sourceId)}/memory`)
  return listOf<TeamMemoryNote>(await readJson(res), 'memory')
}

export async function addTeamMemory(sourceId: string, note: TeamMemoryNote): Promise<void> {
  await readJson(
    await send(`/api/skill-sources/${encodeURIComponent(sourceId)}/memory`, 'POST', note),
  )
}

export async function updateTeamMemory(
  sourceId: string,
  slug: string,
  note: { title: string; body: string },
): Promise<void> {
  await readJson(
    await send(
      `/api/skill-sources/${encodeURIComponent(sourceId)}/memory/${encodeURIComponent(slug)}`,
      'PUT',
      note,
    ),
  )
}

export async function deleteTeamMemory(sourceId: string, slug: string): Promise<void> {
  await readJson(
    await send(
      `/api/skill-sources/${encodeURIComponent(sourceId)}/memory/${encodeURIComponent(slug)}`,
      'DELETE',
    ),
  )
}

export async function getSessionSkills(sessionId: string): Promise<SessionSkills> {
  const body = await readJson<Partial<SessionSkills>>(
    await authedFetch(`/api/sessions/${encodeURIComponent(sessionId)}/skills`),
  )
  return { skills: body?.skills ?? [], groups: body?.groups ?? [], resolved: body?.resolved ?? [] }
}

export async function putSessionSkills(
  sessionId: string,
  sel: { skills: string[]; groups: string[] },
): Promise<SessionSkills> {
  const body = await readJson<Partial<SessionSkills> | undefined>(
    await send(`/api/sessions/${encodeURIComponent(sessionId)}/skills`, 'PUT', sel),
  )
  return {
    skills: body?.skills ?? sel.skills,
    groups: body?.groups ?? sel.groups,
    resolved: body?.resolved ?? [],
  }
}

/** `POST /api/skills/{id}/publish`. A team target commits + pushes the
 *  newest version; the public target publishes it to Peckboard Public (the
 *  hosted catalog) and returns its `url`. A content-check rejection, local
 *  or on the public side, throws `GuardError`. */
export interface PublishResult {
  /** Repo path, `skills/<name>/<version>/SKILL.md`. */
  path?: string
  source_id?: string
  /** Team: false when the repo already held this exact version. */
  committed?: boolean
  /** Public: the hub accepted it. */
  published?: boolean
  name?: string
  version?: string
  /** Public: where the published skill can be viewed. */
  url?: string
}

export async function publishSkill(id: string, targetSourceId: string): Promise<PublishResult> {
  return readJson<PublishResult>(
    await send(`/api/skills/${encodeURIComponent(id)}/publish`, 'POST', {
      target_source_id: targetSourceId,
    }),
  )
}
// ── Peckboard Public: handle, forks, merge requests, history ─────────

/** A hub handle: what this install's forks are named after. */
export const HANDLE_RE = /^[a-z0-9-]{2,32}$/

export interface HubHandle {
  public_key: string
  handle: string
}

export async function getHubHandle(): Promise<HubHandle> {
  return readJson<HubHandle>(await authedFetch('/api/skills/hub/handle'))
}

export async function setHubHandle(handle: string): Promise<HubHandle> {
  return readJson<HubHandle>(await send('/api/skills/hub/handle', 'PUT', { handle }))
}

/** `name@handle` → its parts; null for an original skill. */
export function forkParts(name: string): { base: string; handle: string } | null {
  const i = name.indexOf('@')
  if (i <= 0) return null
  return { base: name.slice(0, i), handle: name.slice(i + 1) }
}

export interface HubSkillDetail {
  name: string
  owner_handle: string
  owner_key: string
  /** A fork's original. */
  parent?: string | null
  latest: string | null
  versions: string[]
  description: string
  /** This install owns it. */
  mine: boolean
  /** This install's fork of it, if it has one. */
  my_fork: string | null
}

export async function getHubSkill(name: string): Promise<HubSkillDetail> {
  const d = await readJson<HubSkillDetail>(
    await authedFetch(`/api/skills/hub/skills/${encodeURIComponent(name)}`),
  )
  return { ...d, versions: d.versions ?? [], my_fork: d.my_fork ?? null, mine: !!d.mine }
}

export interface HubCommit {
  commit: string
  parents: string[]
  author_key: string
  author_handle: string
  message: string
  at: string
  /** The version this commit published, if any. */
  tag?: string | null
}

export async function getHubHistory(name: string): Promise<HubCommit[]> {
  const res = await authedFetch(`/api/skills/hub/skills/${encodeURIComponent(name)}/history`)
  return listOf<HubCommit>(await readJson(res), 'history')
}

export async function getHubDiff(name: string, from: string, to: string): Promise<string> {
  const res = await authedFetch(
    `/api/skills/hub/skills/${encodeURIComponent(name)}/diff${qs({ from, to })}`,
  )
  return (await readJson<{ diff?: string }>(res))?.diff ?? ''
}

/** Fork a public skill (or reuse this install's fork); returns the local,
 *  editable copy. */
export async function forkSkill(name: string): Promise<{ fork: string; skill: Skill }> {
  return readJson(await send('/api/skills/hub/fork', 'POST', { name }))
}

export type SyncStatus = 'up_to_date' | 'fast_forward' | 'merged' | 'conflict'

export async function syncFork(
  fork: string,
): Promise<{ status: SyncStatus; local_updated: boolean; skill: Skill | null }> {
  return readJson(await send(`/api/skills/hub/forks/${encodeURIComponent(fork)}/sync`, 'POST'))
}

export type MrState = 'open' | 'merged' | 'closed'
export type MrStateFilter = MrState | 'all'
export type MrRole = 'incoming' | 'outgoing'

export interface MrComment {
  id: number
  author_handle: string
  author_key: string
  body: string
  at: string
}

export interface MergeRequest {
  id: number
  target: string
  fork: string
  title: string
  description: string
  state: MrState
  author_key: string
  author_handle: string
  target_owner_key: string
  base_commit: string
  head_commit: string
  mergeable: boolean
  /** Detail only. */
  diff?: string
  comments?: MrComment[]
  created_at: string
  updated_at: string
  merged_commit: string | null
  merged_version: string | null
}

export async function listMergeRequests(
  role: MrRole,
  state: MrStateFilter,
): Promise<MergeRequest[]> {
  const res = await authedFetch(`/api/skills/hub/merge-requests${qs({ role, state })}`)
  return listOf<MergeRequest>(await readJson(res), 'merge_requests')
}

export async function getMergeRequest(id: number): Promise<MergeRequest> {
  return readJson<MergeRequest>(await authedFetch(`/api/skills/hub/merge-requests/${id}`))
}

export async function createMergeRequest(
  fork: string,
  title: string,
  description: string,
): Promise<{ merge_request: MergeRequest; published_version: string | null }> {
  return readJson(
    await send('/api/skills/hub/merge-requests', 'POST', { fork, title, description }),
  )
}

export async function commentMergeRequest(id: number, body: string): Promise<MrComment> {
  return readJson(await send(`/api/skills/hub/merge-requests/${id}/comments`, 'POST', { body }))
}

export async function mergeMergeRequest(
  id: number,
  opts: { publish: boolean; bump?: VersionBump },
): Promise<{ merge_request: MergeRequest; local_updated: boolean }> {
  return readJson(await send(`/api/skills/hub/merge-requests/${id}/merge`, 'POST', opts))
}

export async function closeMergeRequest(id: number): Promise<MergeRequest> {
  return readJson(await send(`/api/skills/hub/merge-requests/${id}/close`, 'POST'))
}

/** Open merge requests into skills this install owns — the Merge requests
 *  tab badge. Refreshed on demand and by the `skills-merge-requests` WS
 *  event the server's poller broadcasts. */
export const useMergeRequestBadge = create<{
  open: number
  setOpen: (n: number) => void
  check: () => Promise<void>
}>((set) => ({
  open: 0,
  setOpen: (n) => set({ open: n }),
  check: async () => {
    try {
      const res = await readJson<{ open?: number }>(
        await send('/api/skills/hub/merge-requests/check', 'POST'),
      )
      set({ open: res?.open ?? 0 })
    } catch {
      // No public source, or the hub is unreachable: no badge.
    }
  },
}))

// ── Cached state: installed skills, groups, sources ──────────────────

interface SkillsState {
  skills: Skill[]
  groups: SkillGroup[]
  sources: SkillSource[]
  skillsLoaded: boolean
  groupsLoaded: boolean
  sourcesLoaded: boolean
  error: string | null

  fetchSkills: () => Promise<void>
  fetchGroups: () => Promise<void>
  fetchSources: () => Promise<void>
  /** Everything the Skills page and the pickers read. */
  fetchAll: () => Promise<void>

  createSkill: (input: {
    name: string
    description: string
    tags: string[]
    groups: string[]
    body: string
  }) => Promise<Skill>
  updateSkill: (
    id: string,
    patch: {
      description?: string
      tags?: string[]
      groups?: string[]
      body?: string
      bump?: VersionBump
    },
  ) => Promise<Skill>
  deleteSkill: (id: string) => Promise<void>

  subscribe: (sourceId: string, name: string, version: string | null) => Promise<void>
  setPinnedVersion: (id: string, version: string | null) => Promise<void>
  unsubscribe: (id: string) => Promise<void>

  saveGroup: (
    id: string | null,
    group: { slug: string; name: string; description: string; skills: string[] },
  ) => Promise<void>
  deleteGroup: (id: string) => Promise<void>

  addSource: (src: { kind: 'team'; name: string; url: string; branch?: string }) => Promise<void>
  deleteSource: (id: string) => Promise<void>
  syncSource: (id: string) => Promise<void>
}

const normSkill = (s: Skill): Skill => ({
  ...s,
  tags: s.tags ?? [],
  groups: s.groups ?? [],
  versions: s.versions ?? [],
  pinned_version: s.pinned_version ?? null,
})

export const useSkillsStore = create<SkillsState>((set, get) => ({
  skills: [],
  groups: [],
  sources: [],
  skillsLoaded: false,
  groupsLoaded: false,
  sourcesLoaded: false,
  error: null,

  fetchSkills: async () => {
    try {
      const body = await readJson(await authedFetch('/api/skills'))
      set({ skills: listOf<Skill>(body, 'skills').map(normSkill), skillsLoaded: true, error: null })
    } catch (e) {
      set({ skillsLoaded: true, error: e instanceof Error ? e.message : 'Failed to load skills' })
    }
  },
  fetchGroups: async () => {
    try {
      const body = await readJson(await authedFetch('/api/skill-groups'))
      set({
        groups: listOf<SkillGroup>(body, 'groups').map((g) => ({ ...g, skills: g.skills ?? [] })),
        groupsLoaded: true,
      })
    } catch (e) {
      set({ groupsLoaded: true, error: e instanceof Error ? e.message : 'Failed to load groups' })
    }
  },
  fetchSources: async () => {
    try {
      const body = await readJson(await authedFetch('/api/skill-sources'))
      set({ sources: listOf<SkillSource>(body, 'sources'), sourcesLoaded: true })
    } catch (e) {
      set({ sourcesLoaded: true, error: e instanceof Error ? e.message : 'Failed to load sources' })
    }
  },
  fetchAll: async () => {
    await Promise.all([get().fetchSkills(), get().fetchGroups(), get().fetchSources()])
  },

  createSkill: async (input) => {
    const skill = normSkill(await readJson<Skill>(await send('/api/skills', 'POST', input)))
    await Promise.all([get().fetchSkills(), get().fetchGroups()])
    return skill
  },
  updateSkill: async (id, patch) => {
    const skill = normSkill(
      await readJson<Skill>(await send(`/api/skills/${encodeURIComponent(id)}`, 'PUT', patch)),
    )
    await Promise.all([get().fetchSkills(), get().fetchGroups()])
    return skill
  },
  deleteSkill: async (id) => {
    await readJson(await send(`/api/skills/${encodeURIComponent(id)}`, 'DELETE'))
    set((s) => ({ skills: s.skills.filter((k) => k.id !== id) }))
  },

  subscribe: async (sourceId, name, version) => {
    await readJson(
      await send('/api/skills/subscribe', 'POST', { source_id: sourceId, name, version }),
    )
    await get().fetchSkills()
  },
  setPinnedVersion: async (id, version) => {
    await readJson(
      await send(`/api/skills/${encodeURIComponent(id)}/subscription`, 'PUT', { version }),
    )
    await get().fetchSkills()
  },
  unsubscribe: async (id) => {
    await readJson(await send(`/api/skills/${encodeURIComponent(id)}/subscription`, 'DELETE'))
    set((s) => ({ skills: s.skills.filter((k) => k.id !== id) }))
  },

  saveGroup: async (id, group) => {
    await readJson(
      id
        ? await send(`/api/skill-groups/${encodeURIComponent(id)}`, 'PUT', group)
        : await send('/api/skill-groups', 'POST', group),
    )
    await Promise.all([get().fetchGroups(), get().fetchSkills()])
  },
  deleteGroup: async (id) => {
    await readJson(await send(`/api/skill-groups/${encodeURIComponent(id)}`, 'DELETE'))
    set((s) => ({ groups: s.groups.filter((g) => g.id !== id) }))
  },

  addSource: async (src) => {
    await readJson(await send('/api/skill-sources', 'POST', src))
    await get().fetchSources()
  },
  deleteSource: async (id) => {
    await readJson(await send(`/api/skill-sources/${encodeURIComponent(id)}`, 'DELETE'))
    set((s) => ({ sources: s.sources.filter((x) => x.id !== id) }))
  },
  syncSource: async (id) => {
    try {
      await readJson(await send(`/api/skill-sources/${encodeURIComponent(id)}/sync`, 'POST'))
    } finally {
      // The sync result (last_sync_at / last_error) lives on the source row.
      await Promise.all([get().fetchSources(), get().fetchSkills(), get().fetchGroups()])
    }
  },
}))

/** The version a skill is used at: a subscription's pin, else its newest
 *  (`version` is always the head). */
export function inUseVersion(s: Pick<Skill, 'version' | 'pinned_version'>): string {
  return s.pinned_version ?? s.version
}

/** Semver order (ascending); non-numeric parts compare as 0. */
export function compareVersions(a: string, b: string): number {
  const pa = a.split(/[.-]/).map((x) => Number(x) || 0)
  const pb = b.split(/[.-]/).map((x) => Number(x) || 0)
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const d = (pa[i] ?? 0) - (pb[i] ?? 0)
    if (d !== 0) return d
  }
  return 0
}
/** `1.2.3` → its bumped successor. Unparseable input is returned as-is. */
export function bumpVersion(version: string, bump: VersionBump): string {
  const m = /^(\d+)\.(\d+)\.(\d+)/.exec(version)
  if (!m) return version
  const [maj, min, pat] = [Number(m[1]), Number(m[2]), Number(m[3])]
  if (bump === 'major') return `${maj + 1}.0.0`
  if (bump === 'minor') return `${maj}.${min + 1}.0`
  return `${maj}.${min}.${pat + 1}`
}

/** Kebab-case skill / group / note slug, as the contract requires. */
export const SLUG_RE = /^[a-z0-9][a-z0-9-]{0,63}$/

/** Human label for a guard finding, e.g. `Line 4: api_key — "sk-…"`.
 *  Line 0 is the frontmatter (name, description, tags, groups), not a body
 *  line. */
export function findingLabel(f: GuardFinding): string {
  const where = f.line === 0 ? 'Metadata' : `Line ${f.line}`
  return `${where}: ${f.kind}${f.excerpt ? ` — "${f.excerpt}"` : ''}`
}
