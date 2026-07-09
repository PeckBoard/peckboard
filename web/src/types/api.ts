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
  /** Cost-aware model auto-switch opt-in. null = inherit the default (ON
   *  for worker sessions, OFF for chats); true/false forces it. */
  model_autoswitch?: boolean | null
  is_permanent: boolean
  repeating_task_id: string | null
  /** Target model while a provider/account handover is mid-flight; null otherwise. */
  handover_to_model?: string | null
  /** Handover doc awaiting the incoming model's first turn; null otherwise. */
  pending_handover_doc?: string | null
  /** Latest context-window occupancy (tokens) from the session's usage
   *  rows — present on GET /api/sessions/:id only; live updates arrive via
  /** Latest context-window occupancy (tokens) from the session's usage
   *  rows — present on GET /api/sessions/:id only; live updates arrive via
   *  streamed `agent-usage` events. */
  context_tokens?: number
  /** Named system prompt to inject at the top of the context. */
  system_prompt_name?: string | null
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

export interface Plan {
  id: string
  session_id: string
  card_id: string | null
  project_id: string | null
  title: string
  markdown: string
  /** proposed | commenting | revising | approved | implementing | implemented | reviewed */
  status: string
  version: number
  created_at: string
  updated_at: string
}

export interface PlanComment {
  id: string
  plan_id: string
  /** 1-based source-markdown line the comment is attached to. */
  anchor: number
  body: string
  resolved: boolean
  created_at: string
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
  /** Latest context-window occupancy (tokens) of the card's worker session,
   *  seeded by the cards fetch; live updates ride the streamed `agent-usage`
   *  events. Absent for terminal cards and cards without a worker session. */
  context_tokens?: number
  /** Cost-aware model auto-switch opt-in for this card's workers. null =
   *  inherit the default (ON — cards spawn workers); true/false forces it. */
  model_autoswitch?: boolean | null
  depends_on?: string[]
  created_at: string
  updated_at: string
  completed_at: string | null
  /** Named system prompt to inject at the top of this card's worker context. */
  system_prompt_name?: string | null
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

/** How a Claude account stores its credential — selects the env var the
 *  spawned CLI authenticates through. */
export type ClaudeAccountKind = 'api_key' | 'oauth_token'

/** How close an account is to its budget; mirrors the backend `WarnLevel`. */
export type WarnLevel = 'none' | 'ok' | 'warning' | 'critical' | 'exceeded'

export interface ClaudeAccountUsage {
  total_tokens: number
  est_cost_usd: number
  turns: number
  /** Fraction of budget consumed (max of token/cost), or null if no budget. */
  used_fraction: number | null
  level: WarnLevel
}

/** One subscription plan-usage bucket (the `claude /usage` numbers):
 *  percent of the enforced limit consumed, and when the bucket resets. */
export interface PlanBucket {
  utilization: number
  resets_at: string | null
}

/** The stable buckets the plan-usage endpoint reports. */
export interface PlanUsageBuckets {
  five_hour: PlanBucket | null
  seven_day: PlanBucket | null
  seven_day_sonnet: PlanBucket | null
  seven_day_opus: PlanBucket | null
}

/** Cached plan usage for one login (`default` = host login, else account
 *  id). `usage` is the last good snapshot; `last_error` is set when the
 *  most recent refresh failed (the snapshot may then be stale). */
export interface PlanUsageEntry {
  usage: PlanUsageBuckets | null
  fetched_at: number | null
  last_error: string | null
}

/** Per-login plan usage keyed by `default` or account id. */
export type PlanUsageMap = Record<string, PlanUsageEntry>
/** One logged-in Claude/Anthropic account. The credential itself is never
 *  returned — only a masked `credential_hint`. */
export interface ClaudeAccount {
  id: string
  name: string
  kind: ClaudeAccountKind
  credential_hint: string
  config_dir: string | null
  budget_window_hours: number | null
  budget_limit_usd: number | null
  budget_limit_tokens: number | null
  warn_threshold: number
  critical_threshold: number
  created_at: number
  updated_at: number
  usage: ClaudeAccountUsage
}

/** A finished browser login (`claude setup-token` equivalent): the pasted
 *  `code#state` plus the PKCE material from `login/start`. The server
 *  exchanges it for the long-lived token, so the token never reaches here. */
export interface ClaudeLogin {
  code: string
  verifier: string
  state: string
}

/** The authorize URL + PKCE material returned by `POST
 *  /api/claude-accounts/login/start`. */
export interface ClaudeLoginStart {
  url: string
  verifier: string
  state: string
}

/** Body for creating/updating a Claude account. On update, an empty/omitted
 *  `credential` leaves the stored secret untouched. When `login` is set, the
 *  server exchanges it for the credential and the account is `oauth_token`. */
export interface ClaudeAccountInput {
  name: string
  kind: ClaudeAccountKind
  credential: string
  login?: ClaudeLogin
  budget_window_hours: number | null
  budget_limit_usd: number | null
  budget_limit_tokens: number | null
  warn_threshold: number
  critical_threshold: number
}

/** How a Grok account authenticates the spawned CLI: `device` is a browser
 *  device-code sign-in (`grok login --device-auth`); `api_key` injects an
 *  `XAI_API_KEY`. */
export type GrokAccountKind = 'device' | 'api_key'

export interface GrokAccountUsage {
  total_tokens: number
  est_cost_usd: number
  turns: number
  used_fraction: number | null
  level: WarnLevel
}

/** One Grok/xAI account. The credential is never returned; `authenticated`
 *  reports whether a device account has finished its browser sign-in (or an
 *  api_key account has a key set). */
export interface GrokAccount {
  id: string
  name: string
  kind: GrokAccountKind
  authenticated: boolean
  config_dir: string | null
  budget_window_hours: number | null
  budget_limit_usd: number | null
  budget_limit_tokens: number | null
  warn_threshold: number
  critical_threshold: number
  created_at: number
  updated_at: number
  usage: GrokAccountUsage
}

/** The device-login URL returned by `POST /api/grok-accounts/{id}/login/start`.
 *  The user opens it and authorises in the browser; the `grok` CLI completes
 *  the sign-in on the server side. */
export interface GrokLoginStart {
  url: string
}

/** Body for creating/updating a Grok account. `credential` is only meaningful
 *  for an `api_key` account; on update an empty/omitted value keeps the key. */
export interface GrokAccountInput {
  name: string
  kind: GrokAccountKind
  credential: string
  budget_window_hours: number | null
  budget_limit_usd: number | null
  budget_limit_tokens: number | null
  warn_threshold: number
  critical_threshold: number
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
