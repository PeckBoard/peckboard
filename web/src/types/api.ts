// API contract types matching the backend schema

export interface Folder {
  id: string
  name: string
  path: string
  created_at: string
}

export interface Session {
  id: string
  name: string
  folder_id: string
  model: string | null
  effort: string | null
  is_worker: boolean
  project_id: string | null
  card_id: string | null
  conversation_id: string | null
  created_at: string
  last_activity: string
  is_expert: boolean
  expert_kind: string | null
  knowledge_summary: string | null
  knowledge_area: string | null
  scope_path: string | null
  is_permanent: boolean
  repeating_task_id: string | null
}

export type RepeatingScheduleKind = 'interval' | 'daily' | 'weekly'

export interface RepeatingTask {
  id: string
  name: string
  description: string
  folder_id: string
  prompt: string
  schedule_kind: RepeatingScheduleKind
  /** JSON-encoded schedule value; see backend Schedule::parse. */
  schedule_value: string
  model: string | null
  effort: string | null
  enabled: boolean
  next_run_at: string | null
  last_run_at: string | null
  created_at: string
  updated_at: string
}

export interface Project {
  id: string
  name: string
  context: string
  folder_id: string
  worker_count: number
  status: string
  workflow: string
  model: string | null
  effort: string | null
  parallel_instructions: boolean
  auto_notify_changes: boolean
  worker_communication: boolean
  created_at: string
  last_accessed_at: string
  /** Human-readable reason the project is paused, set automatically when
   *  a card's worker keeps crashing. Null when the project was paused
   *  manually or is active. Cleared on resume. */
  pause_reason: string | null
}

export interface Card {
  id: string
  project_id: string
  title: string
  description: string
  step: string
  priority: number
  workflow: string
  model: string | null
  effort: string | null
  worker_session_id: string | null
  last_worker_session_id: string | null
  handoff_context: string | null
  blocked: boolean
  block_reason: string | null
  depends_on?: string[]
  created_at: string
  updated_at: string
  completed_at: string | null
}

export interface Event {
  id: string
  session_id: string
  seq: number
  ts: number
  kind: string
  data: Record<string, unknown>
}

export interface User {
  id: string
  username: string
  email: string | null
  role: string
  created_at: string
  updated_at: string
}

export interface AuthSession {
  id: string
  user_id: string
  created_at: number
  expires_at: number
  last_used_at: number | null
  user_agent: string | null
  ip_address: string | null
}

export interface PushSubscription {
  endpoint: string
  p256dh: string
  auth_key: string
  created_at: string
}

export interface QueuedMessage {
  session_id: string
  text: string
  queued_at: string
}

export interface Announcement {
  id: string
  kind: string
  title: string
  message: string
  detail: string | null
  created_at: string
}

// Common API response types
export interface ApiError {
  error: string
}

export interface HealthResponse {
  ok: boolean
}

// ── Usage dashboard ──────────────────────────────────────────────────
// Mirrors the Rust contract in `src/routes/usage/mod.rs` (and the cost
// model in `src/routes/usage/cost.rs`). snake_case throughout, matching
// the backend's bare serde field names. Cost figures (`est_cost`) are USD;
// `*_ts` / `bucket_ts` fields are epoch milliseconds.

/** What an `EntityUsage` row aggregates over. */
export type UsageEntityKind = 'session' | 'project' | 'card' | 'expert'

/** What kind of operation an `OperationCost` attributes spend to. */
export type UsageOperationKind = 'file_update' | 'file_read' | 'ask_expert' | 'qa'

/** USD-per-million-token rates for one model, by token kind, as advertised
 *  by the running binary's cost table. */
export interface ModelRates {
  input_per_mtok: number
  output_per_mtok: number
  cache_read_per_mtok: number
  cache_creation_per_mtok: number
}

/** The per-model rate table from `GET /api/usage/costs`, keyed by bare
 *  model id. Fetch once at boot and cache it; never hardcode rates on the
 *  client — price trends with `tokenCost` from `util/cost` so the numbers
 *  match the backend's `est_cost`. */
export interface CostTable {
  rates: Record<string, ModelRates>
}

/** Token totals + estimated cost for one entity (session/project/card/
 *  expert). */
export interface EntityUsage {
  id: string
  name: string
  kind: UsageEntityKind
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_creation_tokens: number
  /** Provider-reported turn total. Overlaps the four billed slices, so it
   *  is a display roll-up only and is never re-priced. */
  total_tokens: number
  /** Latest context-window occupancy snapshot for the entity. */
  context_tokens: number
  /** Estimated cost in USD, computed by the backend from the cost table. */
  est_cost: number
  /** Owning project id — present (non-null) only for `kind: 'card'` rows, so
   *  the cards panel can filter to a selected project; `null`/absent for
   *  session/project/expert kinds. Optional so consumers that never read it
   *  are unaffected. */
  project_id?: string | null
}

/** A session row: `EntityUsage` plus its explicit lifetime totals and role
 *  flags (so the dashboard can split chats / workers / experts and route to
 *  the right detail page). For session rows `project_id` is the owning
 *  project, when any. */
export interface SessionUsage extends EntityUsage {
  total_tokens_used: number
  total_context_tokens: number
  is_worker: boolean
  is_expert: boolean
}

/** One model's share of a multi-model turn. Mirrors `TurnModelUsage` in
 *  `src/routes/usage/turns.rs`. */
export interface TurnModelUsage {
  model: string | null
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_creation_tokens: number
  total_tokens: number
}

/** One turn ("prompt") of a session, from
 *  `GET /api/usage/sessions/{id}/turns`. Mirrors `TurnUsage` in
 *  `src/routes/usage/turns.rs`. */
export interface TurnUsage {
  turn_seq: number | null
  /** End-of-turn timestamp (epoch ms). */
  ts: number
  /** The turn's main model; per-model slices live in `models` when the
   *  turn used more than one. */
  model: string | null
  /** Per-model breakdown; populated only when the turn used >1 model. */
  models: TurnModelUsage[]
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_creation_tokens: number
  total_tokens: number
  /** Context-window occupancy at the end of this turn. */
  context_tokens: number
  est_cost: number
  /** Snippet of the user prompt that started the turn, when one exists. */
  prompt: string | null
  /** Distinct files `Read` during the turn — what its cache-read tokens were
   *  spent re-loading. */
  files_read: string[]
  /** Distinct files edited during the turn. */
  files_edited: string[]
}

/** Cost attributed to a single operation — one file update, one
 *  `ask_expert` round-trip, or one question/answer combination. `ref_id`
 *  points at the underlying thing (file path, expert id, decision id). */
export interface OperationCost {
  kind: UsageOperationKind
  ref_id: string
  label: string
  tokens: number
  est_cost: number
  ts: number
}

/** One point in a usage time-series. `bucket_ts` is the epoch-ms bucket
 *  start. */
export interface TrendPoint {
  bucket_ts: number
  tokens: number
  est_cost: number
}

/** A named time-series for one entity — e.g. `metric: 'tokens'` or
 *  `'cost'` over time. */
export interface TrendSeries {
  metric: string
  entity_id: string
  points: TrendPoint[]
}

/** Install-wide token + cost totals, summed across every entity. */
export interface UsageTotals {
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_creation_tokens: number
  total_tokens: number
  context_tokens: number
  est_cost: number
}

/** Single-fetch envelope for the whole usage dashboard view: totals, the
 *  per-entity breakdowns, the per-operation cost list, and the trend
 *  series the charts render. */
export interface UsageDashboard {
  totals: UsageTotals
  sessions: SessionUsage[]
  projects: EntityUsage[]
  cards: EntityUsage[]
  experts: EntityUsage[]
  operations: OperationCost[]
  trends: TrendSeries[]
}
