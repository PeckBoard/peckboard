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
}

export interface Project {
  id: string
  name: string
  context: string
  folder_id: string
  worker_count: number
  status: string
  default_workflow: string | null
  model: string | null
  effort: string | null
  parallel_instructions: boolean
  auto_notify_changes: boolean
  worker_communication: boolean
  created_at: string
  last_accessed_at: string
}

export interface Card {
  id: string
  project_id: string
  title: string
  description: string
  step: string
  priority: number
  workflow: string | null
  model: string | null
  effort: string | null
  worker_session_id: string | null
  last_worker_session_id: string | null
  handoff_context: string | null
  blocked: boolean
  block_reason: string | null
  created_at: string
  updated_at: string
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
