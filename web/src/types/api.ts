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

/** An expert session as returned by `GET /api/experts`. The endpoint
 *  serializes the full `Session` row, so an Expert is structurally a
 *  Session that is always `is_expert: true`. The fields below are the
 *  ones the Expert Sessions view reads; `expert_kind` is 'knowledge' or
 *  'question'. A null `project_id` means the expert is global (available
 *  to chat sessions across the whole install). */
export type Expert = Session

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

/** An answered PM decision as serialized by
 *  GET /api/projects/:id/pm/decisions (and the answer/edit mutation
 *  responses). `asked_by_session_id` is the provenance of the question
 *  (which worker escalated it); null when the PM expert asked directly. */
export interface PmDecision {
  id: string
  question: string
  answer: string | null
  status: string
  decided_at: string | null
  asked_by_session_id: string | null
  asked_at: string
}

/** A PM question awaiting a user answer, from
 *  GET /api/projects/:id/pm/questions. */
export interface PmPendingQuestion {
  id: string
  question: string
  asked_by_session_id: string | null
  asked_at: string
}

/** Payload of the `pm-decisions-changed` WebSocket event, broadcast
 *  after every PM decision-log mutation. */
export interface PmDecisionsChangedEvent {
  projectId: string
  action: string
  pending_count: number
}

// Common API response types
export interface ApiError {
  error: string
}

export interface HealthResponse {
  ok: boolean
}
