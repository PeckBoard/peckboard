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

/** The operation kinds the cost-breakdown panel aggregates. `GET
 *  /api/usage/operations` takes one `kind` per call, so the store fans out a
 *  request per kind and concatenates. */
const OPERATION_KINDS: UsageOperationKind[] = ['file_update', 'file_read', 'ask_expert', 'qa']

/** GET a usage endpoint, falling back to `fallback` on any non-2xx or network
 *  error. The usage dashboard ships ahead of (and in parallel with) the
 *  backend aggregation, so a route that isn't serving data yet must degrade to
 *  an empty panel rather than crash the whole view. */
async function getJson<T>(url: string, fallback: T): Promise<T> {
  try {
    const res = await authedFetch(url)
    if (!res.ok) return fallback
    return (await res.json()) as T
  } catch {
    return fallback
  }
}

/** Roll the per-session rows up into install-wide totals. The backend has no
 *  single totals endpoint yet; summing the per-session rows is the
 *  non-double-counting source of truth, since the project/card/expert rollups
 *  are just re-groupings of the same underlying session spend. */
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
  /** Last hard error (only set if the whole assembly throws; individual
   *  endpoints degrade to empty rather than erroring). */
  error: string
  fetchUsage: () => Promise<void>
}

export const useUsageStore = create<UsageState>((set) => ({
  costTable: EMPTY_COST_TABLE,
  dashboard: EMPTY_DASHBOARD,
  loaded: false,
  loading: false,
  error: '',

  fetchUsage: async () => {
    set({ loading: true, error: '' })
    try {
      const [costTable, sessions, projects, cards, experts, trends] = await Promise.all([
        getJson<CostTable>('/api/usage/costs', EMPTY_COST_TABLE),
        getJson<SessionUsage[]>('/api/usage/sessions', []),
        getJson<EntityUsage[]>('/api/usage/projects', []),
        getJson<EntityUsage[]>('/api/usage/cards', []),
        getJson<EntityUsage[]>('/api/usage/experts', []),
        getJson<TrendSeries[]>('/api/usage/trends', []),
      ])
      const opLists = await Promise.all(
        OPERATION_KINDS.map((kind) =>
          getJson<OperationCost[]>(`/api/usage/operations?kind=${kind}`, []),
        ),
      )
      const operations = opLists.flat()

      set({
        costTable,
        dashboard: {
          totals: sumTotals(sessions),
          sessions,
          projects,
          cards,
          experts,
          operations,
          trends,
        },
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
