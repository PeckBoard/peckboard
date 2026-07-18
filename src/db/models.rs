use diesel::prelude::*;
use serde::{Deserialize, Serialize};

use super::schema::*;

// ── Folders ──────────────────────────────────────────────────────────

#[derive(Queryable, Selectable, Serialize, Debug, Clone)]
#[diesel(table_name = folders)]
pub struct Folder {
    pub id: String,
    pub name: String,
    pub path: String,
    pub created_at: String,
}

#[derive(Insertable, Deserialize, Debug)]
#[diesel(table_name = folders)]
pub struct NewFolder {
    pub id: String,
    pub name: String,
    pub path: String,
    pub created_at: String,
}

// ── Sessions ─────────────────────────────────────────────────────────

#[derive(Queryable, Selectable, Serialize, Debug, Clone)]
#[diesel(table_name = sessions)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub folder_id: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub is_worker: bool,
    pub project_id: Option<String>,
    pub card_id: Option<String>,
    pub conversation_id: Option<String>,
    pub created_at: String,
    pub last_activity: String,
    pub is_expert: bool,
    pub expert_kind: Option<String>,
    pub knowledge_summary: Option<String>,
    pub knowledge_area: Option<String>,
    pub scope_path: Option<String>,
    pub is_permanent: bool,
    pub repeating_task_id: Option<String>,
    /// Custom system prompt. When non-empty, it extends the standing
    /// Peckboard system prompt for this session's agent runs (appended after
    /// it, not replacing it). Editable across sessions via the
    /// `set_session_system_prompt` MCP tool.
    pub system_prompt: Option<String>,
    /// Target model id while a model-switch handover is mid-flight. Set when
    /// the user switches to a model whose (provider, account) differs from
    /// the current one; the outgoing model is generating the handover doc.
    /// `Some` means "a switch is pending, don't accept new user turns yet".
    /// Cleared to `None` once the switch finalizes. See [`crate::handover`].
    pub handover_to_model: Option<String>,
    /// Handover document produced by the outgoing model, waiting to be read
    /// by the incoming model on its first turn. Set the moment a switch
    /// finalizes; cleared (consumed) when the next user message under the
    /// new model prepends it. See [`crate::handover`].
    pub pending_handover_doc: Option<String>,
    /// Workflow step this worker session is working on. `None` for
    /// non-worker sessions, and for worker sessions whose card has since
    /// advanced to a different step — at which point the session can no
    /// longer be resumed for the card (the resume link is severed by the
    /// card-update path in `db::crud::cards`).
    pub worker_step: Option<String>,
    /// Owning PeckBoard user. `None` for legacy rows and internally-spawned
    /// sessions that resolve to no single user. Set on every creation path;
    /// the session-control send_message gate treats two sessions as same-user
    /// only when both `user_id`s are `Some` and equal (NULL = non-matching).
    pub user_id: Option<String>,
    /// When (ms epoch) this session's context occupancy was last reset —
    /// stamped on session clear and on handover/compaction finalize.
    /// `latest_context_tokens` ignores usage rows at or before it, so the
    /// context badge and the auto-compaction check don't keep reporting the
    /// pre-reset conversation's occupancy (usage rows are billing history
    /// and are never deleted). `None` = never reset.
    pub context_reset_ts: Option<i64>,
    /// Cost-aware auto-switch opt-in for this session. `None` inherits the
    /// default (ON for worker sessions, OFF for chats); `Some(true/false)`
    /// forces it. Gates the model-control MCP tools. See the frugal-mode
    /// plan and `crate::service::mcp_server::handlers::model_control`.
    pub model_autoswitch: Option<bool>,
    /// Name of the library system prompt (`system_prompts.name`) selected
    /// for this session, if any. Reference/display only; the resolved body
    /// lives in `system_prompt`. `None` = no named prompt selected.
    pub system_prompt_name: Option<String>,
    /// One-shot flag: when true, the session's next turn injects the saved
    /// plan ahead of the user message so the (thinking) model reviews the
    /// completed work against it. Set by the review model-switch, cleared
    /// after a single injection. See [`crate::handover`].
    pub pending_plan_review: bool,
    /// Temporary session: deleted automatically (full delete-session
    /// cleanup) when the last `user_tabs` row pointing at it is closed —
    /// see `routes::me::delete_tab` and the startup sweep in
    /// `routes::sessions`. Cleared by the "Keep session" action.
    pub is_temp: bool,
    /// Parent session that spawned this one via the `spawn_subagent` MCP
    /// tool. `Some` marks the session as a subagent (paired with
    /// `expert_kind = "subagent"`): the completion listener reports its
    /// final message back to this parent. `None` for every other session.
    pub parent_session_id: Option<String>,
    /// When (RFC3339) this subagent's completion was reported to its
    /// parent. `None` while running — such rows count toward the parent's
    /// concurrent-subagent cap. Always `None` for non-subagent sessions.
    pub subagent_completed_at: Option<String>,
}

#[derive(Insertable, Deserialize, Debug, Default)]
#[diesel(table_name = sessions)]
pub struct NewSession {
    pub id: String,
    pub name: String,
    pub folder_id: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub is_worker: bool,
    pub project_id: Option<String>,
    pub card_id: Option<String>,
    pub conversation_id: Option<String>,
    pub created_at: String,
    pub last_activity: String,
    pub is_expert: bool,
    pub expert_kind: Option<String>,
    pub knowledge_summary: Option<String>,
    pub knowledge_area: Option<String>,
    pub scope_path: Option<String>,
    pub is_permanent: bool,
    pub repeating_task_id: Option<String>,
    pub system_prompt: Option<String>,
    pub handover_to_model: Option<String>,
    pub pending_handover_doc: Option<String>,
    pub worker_step: Option<String>,
    pub user_id: Option<String>,
    pub context_reset_ts: Option<i64>,
    pub system_prompt_name: Option<String>,
    pub model_autoswitch: Option<bool>,
    pub is_temp: bool,
    pub parent_session_id: Option<String>,
    pub subagent_completed_at: Option<String>,
}
#[derive(AsChangeset, Deserialize, Debug, Default)]
#[diesel(table_name = sessions)]
pub struct UpdateSession {
    pub name: Option<String>,
    pub model: Option<Option<String>>,
    pub effort: Option<Option<String>>,
    pub project_id: Option<Option<String>>,
    pub card_id: Option<Option<String>>,
    pub conversation_id: Option<Option<String>>,
    pub last_activity: Option<String>,
    pub is_expert: Option<bool>,
    pub expert_kind: Option<Option<String>>,
    pub knowledge_summary: Option<Option<String>>,
    pub knowledge_area: Option<Option<String>>,
    pub scope_path: Option<Option<String>>,
    pub is_permanent: Option<bool>,
    pub system_prompt: Option<Option<String>>,
    pub handover_to_model: Option<Option<String>>,
    pub pending_handover_doc: Option<Option<String>>,
    pub system_prompt_name: Option<Option<String>>,
    pub worker_step: Option<Option<String>>,
    pub context_reset_ts: Option<Option<i64>>,
    pub model_autoswitch: Option<Option<bool>>,
    pub pending_plan_review: Option<bool>,
    pub is_temp: Option<bool>,
}
// ── Repeating Tasks ──────────────────────────────────────────────────

#[derive(Queryable, Selectable, Serialize, Debug, Clone)]
#[diesel(table_name = repeating_tasks)]
pub struct RepeatingTask {
    pub id: String,
    pub name: String,
    pub description: String,
    pub folder_id: String,
    pub prompt: String,
    pub schedule_kind: String,
    pub schedule_value: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub enabled: bool,
    pub next_run_at: Option<String>,
    pub last_run_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Insertable, Debug)]
#[diesel(table_name = repeating_tasks)]
pub struct NewRepeatingTask {
    pub id: String,
    pub name: String,
    pub description: String,
    pub folder_id: String,
    pub prompt: String,
    pub schedule_kind: String,
    pub schedule_value: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub enabled: bool,
    pub next_run_at: Option<String>,
    pub last_run_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(AsChangeset, Debug, Default)]
#[diesel(table_name = repeating_tasks)]
pub struct UpdateRepeatingTask {
    pub name: Option<String>,
    pub description: Option<String>,
    pub folder_id: Option<String>,
    pub prompt: Option<String>,
    pub schedule_kind: Option<String>,
    pub schedule_value: Option<String>,
    pub model: Option<Option<String>>,
    pub effort: Option<Option<String>>,
    pub enabled: Option<bool>,
    pub next_run_at: Option<Option<String>>,
    pub last_run_at: Option<Option<String>>,
    pub updated_at: Option<String>,
}

// ── Projects ─────────────────────────────────────────────────────────

#[derive(Queryable, Selectable, Serialize, Debug, Clone)]
#[diesel(table_name = projects)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub context: String,
    pub folder_id: String,
    pub worker_count: i32,
    pub status: String,
    pub workflow: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub parallel_instructions: bool,
    pub auto_notify_changes: bool,
    pub worker_communication: bool,
    pub worktree_isolation: bool,
    pub created_at: String,
    pub last_accessed_at: String,
    pub pause_reason: Option<String>,
    pub budget_usd_cents: Option<i32>,
    pub budget_period: Option<String>,
}

#[derive(Insertable, Deserialize, Debug)]
#[diesel(table_name = projects)]
pub struct NewProject {
    pub id: String,
    pub name: String,
    pub context: String,
    pub folder_id: String,
    pub worker_count: i32,
    pub status: String,
    pub workflow: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub parallel_instructions: bool,
    pub auto_notify_changes: bool,
    pub worker_communication: bool,
    pub created_at: String,
    pub worktree_isolation: bool,
    pub last_accessed_at: String,
    pub budget_usd_cents: Option<i32>,
    pub budget_period: Option<String>,
}

#[derive(AsChangeset, Deserialize, Debug, Default)]
#[diesel(table_name = projects)]
pub struct UpdateProject {
    pub name: Option<String>,
    pub context: Option<String>,
    pub worker_count: Option<i32>,
    pub status: Option<String>,
    pub workflow: Option<String>,
    pub model: Option<Option<String>>,
    pub effort: Option<Option<String>>,
    pub parallel_instructions: Option<bool>,
    pub auto_notify_changes: Option<bool>,
    pub worker_communication: Option<bool>,
    pub last_accessed_at: Option<String>,
    pub pause_reason: Option<Option<String>>,
    pub budget_usd_cents: Option<Option<i32>>,
    pub worktree_isolation: Option<bool>,
    pub budget_period: Option<Option<String>>,
}

// ── Cards ────────────────────────────────────────────────────────────

#[derive(Queryable, Selectable, Serialize, Debug, Clone)]
#[diesel(table_name = cards)]
pub struct Card {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub description: String,
    pub step: String,
    pub priority: i32,
    pub workflow: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub worker_session_id: Option<String>,
    pub last_worker_session_id: Option<String>,
    pub handoff_context: Option<String>,
    pub blocked: bool,
    pub block_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Cost-aware auto-switch opt-in for workers on this card. `None`
    /// inherits the default (ON — cards spawn workers); `Some` forces it.
    /// Copied onto the spawned worker session's own `model_autoswitch`.
    pub model_autoswitch: Option<bool>,
    pub completed_at: Option<String>,
    /// Name of the library system prompt (`system_prompts.name`) attached to
    /// this card, if any. Resolved to a body and applied to the worker
    /// session's `system_prompt` at spawn. `None` = none attached.
    pub system_prompt_name: Option<String>,
}

#[derive(Insertable, Deserialize, Debug)]
#[diesel(table_name = cards)]
pub struct NewCard {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub description: String,
    pub step: String,
    pub priority: i32,
    pub workflow: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub blocked: bool,
    pub block_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub system_prompt_name: Option<String>,
}

#[derive(AsChangeset, Deserialize, Debug, Default)]
#[diesel(table_name = cards)]
pub struct UpdateCard {
    pub title: Option<String>,
    pub description: Option<String>,
    pub step: Option<String>,
    pub priority: Option<i32>,
    pub workflow: Option<String>,
    pub model: Option<Option<String>>,
    pub effort: Option<Option<String>>,
    pub worker_session_id: Option<Option<String>>,
    pub last_worker_session_id: Option<Option<String>>,
    pub handoff_context: Option<Option<String>>,
    pub blocked: Option<bool>,
    pub block_reason: Option<Option<String>>,
    pub updated_at: Option<String>,
    pub completed_at: Option<Option<String>>,
    pub system_prompt_name: Option<Option<String>>,
    pub model_autoswitch: Option<Option<bool>>,
}

// ── Card dependencies ────────────────────────────────────────────────

#[derive(Queryable, Selectable, Serialize, Debug, Clone)]
#[diesel(table_name = card_dependencies)]
pub struct CardDependency {
    pub card_id: String,
    pub depends_on_card_id: String,
    pub created_at: String,
}

#[derive(Insertable, Debug)]
#[diesel(table_name = card_dependencies)]
pub struct NewCardDependency {
    pub card_id: String,
    pub depends_on_card_id: String,
    pub created_at: String,
}

// ── Events ───────────────────────────────────────────────────────────

#[derive(Queryable, Selectable, Serialize, Debug, Clone)]
#[diesel(table_name = events)]
pub struct Event {
    pub id: String,
    pub session_id: String,
    pub seq: i32,
    pub ts: i64,
    pub kind: String,
    pub data: String,
}

#[derive(Insertable, Deserialize, Debug)]
#[diesel(table_name = events)]
pub struct NewEvent {
    pub id: String,
    pub session_id: String,
    pub seq: i32,
    pub ts: i64,
    pub kind: String,
    pub data: String,
}

// ── Users ────────────────────────────────────────────────────────────

#[derive(Queryable, Selectable, Serialize, Debug, Clone)]
#[diesel(table_name = users)]
pub struct User {
    pub id: String,
    pub username: String,
    pub email: Option<String>,
    pub password_hash: String,
    pub role: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Insertable, Deserialize, Debug)]
#[diesel(table_name = users)]
pub struct NewUser {
    pub id: String,
    pub username: String,
    pub email: Option<String>,
    pub password_hash: String,
    pub role: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(AsChangeset, Deserialize, Debug, Default)]
#[diesel(table_name = users)]
pub struct UpdateUser {
    pub username: Option<String>,
    pub email: Option<Option<String>>,
    pub password_hash: Option<String>,
    pub role: Option<String>,
    pub updated_at: Option<String>,
}

// ── Auth Sessions ────────────────────────────────────────────────────

#[derive(Queryable, Selectable, Serialize, Debug, Clone)]
#[diesel(table_name = auth_sessions)]
pub struct AuthSession {
    pub id: String,
    pub user_id: String,
    pub token_hash: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub last_used_at: Option<i64>,
    pub user_agent: Option<String>,
    pub ip_address: Option<String>,
}

#[derive(Insertable, Deserialize, Debug)]
#[diesel(table_name = auth_sessions)]
pub struct NewAuthSession {
    pub id: String,
    pub user_id: String,
    pub token_hash: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub user_agent: Option<String>,
    pub ip_address: Option<String>,
}

// ── Push Subscriptions ───────────────────────────────────────────────

#[derive(Queryable, Selectable, Serialize, Debug, Clone)]
#[diesel(table_name = push_subscriptions)]
pub struct PushSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth_key: String,
    pub created_at: String,
}

#[derive(Insertable, Deserialize, Debug)]
#[diesel(table_name = push_subscriptions)]
pub struct NewPushSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth_key: String,
    pub created_at: String,
}

// ── Queued Messages ──────────────────────────────────────────────────

#[derive(Queryable, Selectable, Serialize, Debug, Clone)]
#[diesel(table_name = queued_messages)]
pub struct QueuedMessage {
    pub session_id: String,
    pub text: String,
    pub queued_at: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Insertable, Deserialize, Debug, Default)]
#[diesel(table_name = queued_messages)]
pub struct NewQueuedMessage {
    pub session_id: String,
    pub text: String,
    pub queued_at: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}

// ── Announcements ────────────────────────────────────────────────────

#[derive(Queryable, Selectable, Serialize, Debug, Clone)]
#[diesel(table_name = announcements)]
pub struct Announcement {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub message: String,
    pub detail: Option<String>,
    pub created_at: String,
}

#[derive(Insertable, Deserialize, Debug)]
#[diesel(table_name = announcements)]
pub struct NewAnnouncement {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub message: String,
    pub detail: Option<String>,
    pub created_at: String,
}

// ── Todos ────────────────────────────────────────────────────────────

#[derive(Queryable, Selectable, Serialize, Debug, Clone)]
#[diesel(table_name = todos)]
pub struct TodoRow {
    pub session_id: String,
    pub position: i32,
    pub content: String,
    pub status: String,
    pub active_form: Option<String>,
    pub updated_at: String,
}

#[derive(Insertable, Debug)]
#[diesel(table_name = todos)]
pub struct NewTodoRow {
    pub session_id: String,
    pub position: i32,
    pub content: String,
    pub status: String,
    pub active_form: Option<String>,
    pub updated_at: String,
}

// ── User tabs ────────────────────────────────────────────────────────

#[derive(Queryable, Selectable, Insertable, Serialize, Deserialize, Debug, Clone)]
#[diesel(table_name = user_tabs)]
pub struct UserTab {
    pub user_id: String,
    pub item_type: String,
    pub item_id: String,
    pub last_active: String,
}

// ── Project workflow instructions ────────────────────────────────────

#[derive(Queryable, Selectable, Insertable, Serialize, Debug, Clone)]
#[diesel(table_name = project_workflow_instructions)]
pub struct ProjectWorkflowInstruction {
    pub project_id: String,
    pub workflow_id: String,
    pub step: String,
    pub instructions: String,
    pub created_at: String,
    pub updated_at: String,
}

// ── Plugin settings ──────────────────────────────────────────────────

#[derive(Queryable, Selectable, Insertable, Serialize, Debug, Clone)]
#[diesel(table_name = plugin_settings)]
pub struct PluginSettingRow {
    pub plugin_id: String,
    pub key: String,
    pub value: String,
    pub updated_at: String,
}

// ── Plugin document store + session metadata (generic plugin storage) ──

/// One row in a plugin's document store (`plugin_data`). `data` is opaque
/// per-plugin JSON keyed by `(plugin_id, collection, key)`; core never
/// queries into it. Written via the `data_store`-gated host functions.
#[derive(Queryable, Selectable, Insertable, Serialize, Debug, Clone)]
#[diesel(table_name = plugin_data)]
pub struct PluginDataRow {
    pub plugin_id: String,
    pub collection: String,
    pub key: String,
    pub data: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Plugin-namespaced metadata attached to a core session
/// (`plugin_session_meta`). One opaque JSON blob per `(session_id,
/// plugin_id)`, so "what is an expert" lives in the plugin, not core
/// columns. Written via the `session_write`-gated host functions.
#[derive(Queryable, Selectable, Insertable, Serialize, Debug, Clone)]
#[diesel(table_name = plugin_session_meta)]
pub struct PluginSessionMetaRow {
    pub session_id: String,
    pub plugin_id: String,
    pub data: String,
    pub updated_at: String,
}

// ── Plugin approvals ─────────────────────────────────────────────────

/// One operator decision on a WASM plugin's declared hook set. `hooks`
/// is the canonical (sorted, newline-joined) hook list the decision was
/// made against; `status` is `"approved"` or `"denied"`. A plugin whose
/// currently-declared hooks no longer match `hooks` is treated as having
/// no decision (pending) — see `PluginManager::load_plugin`.
#[derive(Queryable, Selectable, Insertable, Serialize, Debug, Clone)]
#[diesel(table_name = plugin_approvals)]
pub struct PluginApprovalRow {
    pub plugin_id: String,
    pub hooks: String,
    pub status: String,
    pub decided_at: String,
}

/// One plugin registry source. `url` is the resolved registry.json URL;
/// `label` is what the operator entered (an `owner/repo` slug or a URL).
#[derive(Queryable, Selectable, Insertable, Serialize, Debug, Clone)]
#[diesel(table_name = plugin_repositories)]
pub struct PluginRepositoryRow {
    pub url: String,
    pub label: String,
    pub added_at: String,
}

// ── Usage events ─────────────────────────────────────────────────────

#[derive(Queryable, Selectable, Serialize, Debug, Clone)]
#[diesel(table_name = usage_events)]
pub struct UsageEvent {
    pub id: String,
    pub session_id: String,
    pub event_id: Option<String>,
    pub turn_seq: Option<i32>,
    pub ts: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub total_tokens: i64,
    pub context_tokens: i64,
    pub model: Option<String>,
    /// The Claude account this turn billed against, or `None` for the
    /// implicit Default account (host credentials). Derived at spawn from
    /// the `@<account_id>` suffix on the session's model id.
    pub account_id: Option<String>,
}

#[derive(Insertable, Debug, Default)]
#[diesel(table_name = usage_events)]
pub struct NewUsageEvent {
    pub id: String,
    pub session_id: String,
    pub event_id: Option<String>,
    /// Left `None` by live callers; [`Db::record_usage_event`] assigns
    /// the next per-session turn number on insert.
    pub turn_seq: Option<i32>,
    pub ts: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub total_tokens: i64,
    pub context_tokens: i64,
    pub model: Option<String>,
    pub account_id: Option<String>,
}

// ── Claude accounts ──────────────────────────────────────────────────

/// One Claude/Anthropic credential the spawned `claude` CLI can run as.
/// `kind` is `"api_key"` or `"oauth_token"`; see the migration for how
/// each maps onto a spawn-time env var. `credential` is stored as-is —
/// the data dir is host-local and the app is password-gated, so this is
/// the same trust boundary as the JWT secret already kept beside it.
#[derive(Queryable, Selectable, Serialize, Debug, Clone)]
#[diesel(table_name = claude_accounts)]
pub struct ClaudeAccount {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub credential: String,
    pub config_dir: Option<String>,
    pub budget_window_hours: Option<i32>,
    pub budget_limit_usd: Option<f64>,
    pub budget_limit_tokens: Option<i64>,
    pub warn_threshold: f64,
    pub critical_threshold: f64,
    pub created_at: i64,
    pub updated_at: i64,
    /// Refresh token for short-lived browser-login credentials; NULL for
    /// `api_key` accounts and legacy long-lived setup tokens.
    pub refresh_token: Option<String>,
    /// ms epoch when `credential` expires; NULL = long-lived.
    pub token_expires_at: Option<i64>,
}

#[derive(Insertable, Debug)]
#[diesel(table_name = claude_accounts)]
pub struct NewClaudeAccount {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub credential: String,
    pub config_dir: Option<String>,
    pub budget_window_hours: Option<i32>,
    pub budget_limit_usd: Option<f64>,
    pub budget_limit_tokens: Option<i64>,
    pub warn_threshold: f64,
    pub critical_threshold: f64,
    pub created_at: i64,
    pub updated_at: i64,
    pub refresh_token: Option<String>,
    pub token_expires_at: Option<i64>,
}

/// Mutable fields of an account. `None` leaves a field unchanged.
/// `credential` is `None` on a metadata-only edit so the UI never has to
/// round-trip the secret back just to rename or rebudget an account.
/// The doubly-wrapped budget fields distinguish "leave as-is" (outer
/// `None`) from "clear the budget" (`Some(None)`).
#[derive(AsChangeset, Debug, Default)]
#[diesel(table_name = claude_accounts)]
pub struct ClaudeAccountChanges {
    pub name: Option<String>,
    pub credential: Option<String>,
    pub budget_window_hours: Option<Option<i32>>,
    pub budget_limit_usd: Option<Option<f64>>,
    pub budget_limit_tokens: Option<Option<i64>>,
    pub warn_threshold: Option<f64>,
    pub critical_threshold: Option<f64>,
    pub updated_at: Option<i64>,
    pub refresh_token: Option<Option<String>>,
    pub token_expires_at: Option<Option<i64>>,
}

/// One Grok / xAI account the spawned `grok` CLI can run as. Mirrors
/// [`ClaudeAccount`]; see the `grok_accounts` migration for how each `kind`
/// (`"device"` / `"api_key"`) authenticates the CLI. For a `device` account
/// the `credential` is just the non-secret marker `"device"` — the real
/// credentials live in `config_dir/auth.json` (the per-account GROK_HOME).
#[derive(Queryable, Selectable, Serialize, Debug, Clone)]
#[diesel(table_name = grok_accounts)]
pub struct GrokAccount {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub credential: String,
    pub config_dir: Option<String>,
    pub budget_window_hours: Option<i32>,
    pub budget_limit_usd: Option<f64>,
    pub budget_limit_tokens: Option<i64>,
    pub warn_threshold: f64,
    pub critical_threshold: f64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Insertable, Debug)]
#[diesel(table_name = grok_accounts)]
pub struct NewGrokAccount {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub credential: String,
    pub config_dir: Option<String>,
    pub budget_window_hours: Option<i32>,
    pub budget_limit_usd: Option<f64>,
    pub budget_limit_tokens: Option<i64>,
    pub warn_threshold: f64,
    pub critical_threshold: f64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Mutable fields of a Grok account. `None` leaves a field unchanged; the
/// doubly-wrapped budget fields distinguish "leave as-is" (outer `None`)
/// from "clear the budget" (`Some(None)`). Mirrors [`ClaudeAccountChanges`].
#[derive(AsChangeset, Debug, Default)]
#[diesel(table_name = grok_accounts)]
pub struct GrokAccountChanges {
    pub name: Option<String>,
    pub credential: Option<String>,
    pub budget_window_hours: Option<Option<i32>>,
    pub budget_limit_usd: Option<Option<f64>>,
    pub budget_limit_tokens: Option<Option<i64>>,
    pub warn_threshold: Option<f64>,
    pub critical_threshold: Option<f64>,
    pub updated_at: Option<i64>,
}

// ── System Prompts ────────────────────────────────────

/// A named, reusable system prompt in the library. `name` is the stable
/// handle callers reference (unique); `source_url` records where an
/// imported prompt came from, `None` for hand-written ones.
#[derive(Queryable, Selectable, Serialize, Debug, Clone)]
#[diesel(table_name = system_prompts)]
pub struct SystemPrompt {
    pub id: String,
    pub name: String,
    pub body: String,
    pub source_url: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Insertable)]
#[diesel(table_name = system_prompts)]
pub struct NewSystemPrompt {
    pub id: String,
    pub name: String,
    pub body: String,
    pub source_url: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

// ── Plans ────────────────────────────────────────

/// A durable plan authored by a session. Survives model switches,
/// termination, and clear_session because it lives in its own table.
#[derive(Queryable, Selectable, Serialize, Debug, Clone)]
#[diesel(table_name = plans)]
pub struct Plan {
    pub id: String,
    /// Creator session (worker or chat).
    pub session_id: String,
    /// Set when the creator is a worker on a card.
    pub card_id: Option<String>,
    pub project_id: Option<String>,
    pub title: String,
    pub markdown: String,
    /// proposed | commenting | revising | approved | implementing |
    /// implemented | reviewed
    pub status: String,
    pub version: i32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Insertable, Debug, Default)]
#[diesel(table_name = plans)]
pub struct NewPlan {
    pub id: String,
    pub session_id: String,
    pub card_id: Option<String>,
    pub project_id: Option<String>,
    pub title: String,
    pub markdown: String,
    pub status: String,
    pub version: i32,
    pub created_at: String,
    pub updated_at: String,
}

/// A per-line human review comment on a proposed plan.
#[derive(Queryable, Selectable, Serialize, Debug, Clone)]
#[diesel(table_name = plan_comments)]
pub struct PlanComment {
    pub id: String,
    pub plan_id: String,
    /// 1-based source-markdown line the comment is attached to.
    pub anchor: i32,
    pub body: String,
    pub resolved: bool,
    pub created_at: String,
}

#[derive(Insertable, Debug, Default)]
#[diesel(table_name = plan_comments)]
pub struct NewPlanComment {
    pub id: String,
    pub plan_id: String,
    pub anchor: i32,
    pub body: String,
    pub resolved: bool,
    pub created_at: String,
}
