import { create } from 'zustand'
import type {
  CostTable,
  EntityUsage,
  OperationCost,
  SessionUsage,
  TrendSeries,
  TurnUsage,
  UsageDashboard,
  UsageOperationKind,
  UsageTotals,
} from '../types/api'
import { authedFetch } from './auth'

// Stable empty sentinels. Returning a fresh `[]`/object from a selector each
// render thrashes zustand subscribers, so the store always hands back these
// shared references when there is no data yet (see EMPTY_DASHBOARD below).
export const EMPTY_TOTALS: UsageTotals = {
  input_tokens: 0,
  output_tokens: 0,
  cache_read_tokens: 0,
  cache_creation_tokens: 0,
  total_tokens: 0,
  context_tokens: 0,
  est_cost: 0,
}

export const EMPTY_DASHBOARD: UsageDashboard = {
  totals: EMPTY_TOTALS,
  sessions: [],
  projects: [],
  cards: [],
  experts: [],
  operations: [],
  trends: [],
}

const EMPTY_COST_TABLE: CostTable = { rates: {} }
/* ─── Date range ──────────────────────────────────────────────────────── */

/** Presets for the dashboard's date-range picker. `all` restores the
 *  historical all-time view; `custom` reads the explicit day bounds. */
export type RangePreset = '24h' | '7d' | '30d' | 'all' | 'custom'

export interface UsageRange {
  preset: RangePreset
  /** Local calendar days (`YYYY-MM-DD`) for `custom`; ignored otherwise. */
  fromDay?: string
  toDay?: string
}

/** A range resolved against a concrete "now": epoch ms, `from` inclusive and
 *  `to` exclusive — the same semantics the backend applies. `from` is
 *  undefined only for `all`, which deliberately sends no lower bound. */
export interface ResolvedRange {
  from?: number
  to: number
}

/** Button labels for the preset picker, in display order. */
export const RANGE_PRESETS: { value: RangePreset; label: string; title: string }[] = [
  { value: '24h', label: '24h', title: 'Last 24 hours' },
  { value: '7d', label: '7d', title: 'Last 7 days' },
  { value: '30d', label: '30d', title: 'Last 30 days' },
  { value: 'all', label: 'All', title: 'All recorded usage' },
  { value: 'custom', label: 'Custom', title: 'Pick a start and end date' },
]

const PRESET_SPAN_MS: Partial<Record<RangePreset, number>> = {
  '24h': 24 * 60 * 60 * 1000,
  '7d': 7 * 24 * 60 * 60 * 1000,
  '30d': 30 * 24 * 60 * 60 * 1000,
}

const RANGE_STORAGE_KEY = 'peckboard_usage_range'

/** The range the dashboard opens on. 30 days rather than all-time: the
 *  figures are now labelled with the window they describe, and a recent
 *  window is the question people actually ask. `All` stays one click away. */
const DEFAULT_RANGE: UsageRange = { preset: '30d' }

const DAY_RE = /^(\d{4})-(\d{2})-(\d{2})$/

/** Midnight local time at the start of a `YYYY-MM-DD` day, `offsetDays` days
 *  later. Built from the parts rather than `Date.parse`, because
 *  `new Date('2026-07-27')` is parsed as UTC and would shift the window by
 *  the viewer's offset; stepping the calendar date also keeps it correct
 *  across a DST boundary, where a day is not 24h. */
function dayStartMs(day: string, offsetDays = 0): number | undefined {
  const m = DAY_RE.exec(day)
  if (!m) return undefined
  return new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3]) + offsetDays).getTime()
}

/** Resolve a range against `now`. For `custom`, `toDay` is inclusive to the
 *  user (they picked a day, not an instant), so the exclusive bound is the
 *  start of the following day. */
export function resolveRange(range: UsageRange, now: number): ResolvedRange {
  if (range.preset === 'custom') {
    return {
      from: range.fromDay ? dayStartMs(range.fromDay) : undefined,
      to: (range.toDay ? dayStartMs(range.toDay, 1) : undefined) ?? now,
    }
  }
  const span = PRESET_SPAN_MS[range.preset]
  return { from: span == null ? undefined : now - span, to: now }
}

/** Write the resolved window onto a query string. */
function appendRange(params: URLSearchParams, r: ResolvedRange) {
  if (r.from != null) params.set('from', String(r.from))
  params.set('to', String(r.to))
}

/** The range is a per-browser view preference, so it lives in localStorage —
 *  not the DB (see AGENTS.md on what actually deserves a migration). */
function loadRange(): UsageRange {
  try {
    const raw = localStorage.getItem(RANGE_STORAGE_KEY)
    if (!raw) return DEFAULT_RANGE
    const parsed = JSON.parse(raw) as UsageRange
    if (parsed && RANGE_PRESETS.some((p) => p.value === parsed.preset)) return parsed
  } catch {
    // Corrupt JSON or unavailable storage: fall back to the default.
  }
  return DEFAULT_RANGE
}

function saveRange(range: UsageRange) {
  try {
    localStorage.setItem(RANGE_STORAGE_KEY, JSON.stringify(range))
  } catch {
    // Private-mode storage failures must not break the dashboard.
  }
}

/** The operation kinds the cost-breakdown panel aggregates. `GET
 *  /api/usage/operations` takes one `kind` per call, so the store fans out a
 *  request per kind and concatenates. */
const OPERATION_KINDS: UsageOperationKind[] = ['file_update', 'file_read', 'ask_expert', 'qa']

/** The dashboard's independently-fetched panels. `fetchUsage` records the ones
 *  whose request failed so each can render an error instead of an empty state. */
export type UsagePanelKey =
  | 'costs'
  | 'sessions'
  | 'projects'
  | 'cards'
  | 'experts'
  | 'trends'
  | 'operations'

/** Human labels for the partial-failure strip. */
export const USAGE_PANEL_LABELS: Record<UsagePanelKey, string> = {
  costs: 'cost rates',
  sessions: 'sessions',
  projects: 'projects',
  cards: 'cards',
  experts: 'experts',
  trends: 'trends',
  operations: 'cost breakdown',
}

/** GET a usage endpoint: the parsed body (or `fallback`) plus whether the
 *  request actually failed. The dashboard fans out across many endpoints and
 *  one dead route must not sink the others — but a failed leg has to stay
 *  distinguishable from a leg that legitimately returned nothing, or the view
 *  claims "no data" when the truth is "we could not load it". */
async function getJsonResult<T>(url: string, fallback: T): Promise<{ data: T; failed: boolean }> {
  try {
    const res = await authedFetch(url)
    if (!res.ok) return { data: fallback, failed: true }
    return { data: (await res.json()) as T, failed: false }
  } catch {
    return { data: fallback, failed: true }
  }
}

/** `getJsonResult` for the one-shot helpers below, whose callers handle an
 *  empty result themselves. */
async function getJson<T>(url: string, fallback: T): Promise<T> {
  return (await getJsonResult(url, fallback)).data
}

/** Roll the per-session rows up into install-wide totals. The backend has no
 *  single totals endpoint yet; summing the per-session rows is the
 *  non-double-counting source of truth, since the project/card/expert rollups
 *  are just re-groupings of the same underlying session spend.
 *
 *  Two summed fields here must NOT be displayed as-is. `context_tokens` is a
 *  per-session occupancy snapshot, so its sum is meaningless — the dashboard
 *  shows the largest single session's context instead. `total_tokens` is the
 *  provider-reported roll-up, which is not the figure the session rows show;
 *  every “tokens” label goes through `billedTokens` (the four billed slices)
 *  so the header card reconciles with the panels. Both are still summed to
 *  keep this a complete `UsageTotals`. */
function sumTotals(sessions: SessionUsage[]): UsageTotals {
  return sessions.reduce<UsageTotals>(
    (acc, s) => ({
      input_tokens: acc.input_tokens + s.input_tokens,
      output_tokens: acc.output_tokens + s.output_tokens,
      cache_read_tokens: acc.cache_read_tokens + s.cache_read_tokens,
      cache_creation_tokens: acc.cache_creation_tokens + s.cache_creation_tokens,
      total_tokens: acc.total_tokens + s.total_tokens,
      context_tokens: acc.context_tokens + s.context_tokens,
      est_cost: acc.est_cost + s.est_cost,
    }),
    { ...EMPTY_TOTALS },
  )
}

interface UsageState {
  /** Per-model rate table from `GET /api/usage/costs`, fetched once and cached
   *  so client-side trend math prices the same way the backend does. */
  costTable: CostTable
  /** The assembled dashboard envelope (totals + per-entity breakdowns +
   *  operations + trends). Always the EMPTY_DASHBOARD sentinel until a fetch
   *  populates it, so consumers never deal with null. */
  dashboard: UsageDashboard
  /** True once `fetchUsage` has completed at least once. Lets the view tell
   *  "still loading" apart from "loaded, but empty". */
  loaded: boolean
  /** True while a fetch is in flight — drives the spinner. */
  loading: boolean
  /** Last hard error (only set if the whole assembly throws). Individual
   *  endpoint failures are reported through `failedPanels` instead. */
  error: string
  /** Which panels' backing requests failed on the last `fetchUsage`. Non-empty
   *  means those panels must show an error + Retry rather than an empty state
   *  the user would read as "I have no spend". */
  failedPanels: UsagePanelKey[]
  /** The date range every panel is scoped to. Persisted per browser. */
  range: UsageRange
  /** The window `range` resolved to on the last fetch — what the figures on
   *  screen actually describe. The caption reads this, not `range`, so it can
   *  never claim a window the data was not fetched for. */
  resolved: ResolvedRange
  /** When the currently-displayed figures were fetched (epoch ms; 0 = never).
   *  Drives the "updated HH:MM:SS" stamp. */
  lastUpdated: number
  /** Swap the range and refetch everything scoped to it. */
  setRange: (range: UsageRange) => void
  fetchUsage: () => Promise<void>
  /** Fetch just the rate table (cheap) — for the chat's per-turn cost chips
   *  without pulling the whole dashboard. No-op once populated. */
  fetchCostTable: () => Promise<void>
}

export const useUsageStore = create<UsageState>((set, get) => ({
  costTable: EMPTY_COST_TABLE,
  dashboard: EMPTY_DASHBOARD,
  loaded: false,
  loading: false,
  error: '',
  failedPanels: [],
  range: loadRange(),
  resolved: resolveRange(loadRange(), Date.now()),
  lastUpdated: 0,

  setRange: (range) => {
    saveRange(range)
    set({ range })
    void get().fetchUsage()
  },

  fetchCostTable: async () => {
    if (Object.keys(get().costTable.rates).length > 0) return
    const costTable = await getJson<CostTable>('/api/usage/costs', EMPTY_COST_TABLE)
    set({ costTable })
  },
  fetchUsage: async () => {
    // Resolve the window once per fetch and keep it: every panel is then
    // fetched for exactly the window the caption will name, even though
    // "now" moves while the requests are in flight.
    const resolved = resolveRange(get().range, Date.now())
    const params = new URLSearchParams()
    appendRange(params, resolved)
    const win = `?${params.toString()}`
    set({ loading: true, error: '' })
    try {
      const [costTable, sessions, projects, cards, experts, trends] = await Promise.all([
        getJsonResult<CostTable>('/api/usage/costs', EMPTY_COST_TABLE),
        getJsonResult<SessionUsage[]>(`/api/usage/sessions${win}`, []),
        getJsonResult<EntityUsage[]>(`/api/usage/projects${win}`, []),
        getJsonResult<EntityUsage[]>(`/api/usage/cards${win}`, []),
        getJsonResult<EntityUsage[]>(`/api/usage/experts${win}`, []),
        getJsonResult<TrendSeries[]>(`/api/usage/trends${win}`, []),
      ])
      const opLists = await Promise.all(
        OPERATION_KINDS.map((kind) =>
          getJsonResult<OperationCost[]>(
            `/api/usage/operations?kind=${kind}&${params.toString()}`,
            [],
          ),
        ),
      )
      const operations = opLists.flatMap((r) => r.data)

      // One entry per panel whose request failed, so the view can name what is
      // missing instead of rendering a confident zero.
      const failedPanels: UsagePanelKey[] = []
      if (costTable.failed) failedPanels.push('costs')
      if (sessions.failed) failedPanels.push('sessions')
      if (projects.failed) failedPanels.push('projects')
      if (cards.failed) failedPanels.push('cards')
      if (experts.failed) failedPanels.push('experts')
      if (trends.failed) failedPanels.push('trends')
      if (opLists.some((r) => r.failed)) failedPanels.push('operations')

      set({
        costTable: costTable.data,
        dashboard: {
          totals: sumTotals(sessions.data),
          sessions: sessions.data,
          projects: projects.data,
          cards: cards.data,
          experts: experts.data,
          operations,
          trends: trends.data,
        },
        resolved,
        lastUpdated: Date.now(),
        failedPanels,
        loaded: true,
        loading: false,
        error: '',
      })
    } catch (err) {
      set({
        loaded: true,
        loading: false,
        error: err instanceof Error ? err.message : 'Failed to load usage',
      })
    }
  },
}))

/** Which entity dimension a trend series is bucketed over. `overall` is a
 *  single install-wide series; the rest yield one series per entity of that
 *  kind (unless `id` narrows to one). `operation` buckets per operation kind. */
export type TrendEntity = 'overall' | 'session' | 'project' | 'card' | 'expert' | 'operation'

export interface TrendQuery {
  /** Which figure labels the series. Every point carries both tokens and
   *  est_cost regardless; this only sets `TrendSeries.metric`. */
  metric: 'tokens' | 'cost'
  entity: TrendEntity
  /** Narrow to a single entity id / operation kind. Omit for one series per
   *  entity of the kind. */
  id?: string
  bucket: 'hour' | 'day'
  /** Inclusive window start, epoch ms. Omit to let the backend default to the
   *  most recent window. */
  from?: number
  /** Exclusive window end, epoch ms. Omit for "now". */
  to?: number
}

/** Per-turn ("per-prompt") breakdown for one session, oldest first. Degrades
 *  to `[]` on error like every other usage read. */
export async function fetchSessionTurns(sessionId: string): Promise<TurnUsage[]> {
  return getJson<TurnUsage[]>(`/api/usage/sessions/${encodeURIComponent(sessionId)}/turns`, [])
}

/** Single-session rollup, or null when unavailable. */
export async function fetchSessionUsage(sessionId: string): Promise<SessionUsage | null> {
  return getJson<SessionUsage | null>(`/api/usage/sessions/${encodeURIComponent(sessionId)}`, null)
}

/** Operation costs of one kind, scoped to a session or project (or the whole
 *  install when no scope is given). */
export async function fetchOperationCosts(
  kind: UsageOperationKind,
  scope?: { sessionId?: string; projectId?: string },
): Promise<OperationCost[]> {
  const params = new URLSearchParams({ kind })
  if (scope?.sessionId) params.set('session_id', scope.sessionId)
  else if (scope?.projectId) params.set('project_id', scope.projectId)
  return getJson<OperationCost[]>(`/api/usage/operations?${params.toString()}`, [])
}

/** Parameterized fetch of `GET /api/usage/trends`, for the trend-chart widgets
 *  whose bucket/entity selectors re-query live. Degrades to `[]` on any
 *  non-2xx or network error, same as the dashboard's other usage reads, so a
 *  not-yet-serving backend renders an empty chart rather than throwing. */
export async function fetchTrendSeries(q: TrendQuery): Promise<TrendSeries[]> {
  const params = new URLSearchParams()
  params.set('metric', q.metric)
  params.set('entity', q.entity)
  params.set('bucket', q.bucket)
  if (q.id) params.set('id', q.id)
  if (q.from != null) params.set('from', String(q.from))
  if (q.to != null) params.set('to', String(q.to))
  return getJson<TrendSeries[]>(`/api/usage/trends?${params.toString()}`, [])
}
