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
    pub created_at: String,
    pub last_accessed_at: String,
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
    pub last_accessed_at: String,
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
    pub workflow: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub worker_session_id: Option<String>,
    pub last_worker_session_id: Option<String>,
    pub handoff_context: Option<String>,
    pub blocked: bool,
    pub block_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
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
    pub workflow: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(AsChangeset, Deserialize, Debug, Default)]
#[diesel(table_name = cards)]
pub struct UpdateCard {
    pub title: Option<String>,
    pub description: Option<String>,
    pub step: Option<String>,
    pub priority: Option<i32>,
    pub workflow: Option<Option<String>>,
    pub model: Option<Option<String>>,
    pub effort: Option<Option<String>>,
    pub worker_session_id: Option<Option<String>>,
    pub last_worker_session_id: Option<Option<String>>,
    pub handoff_context: Option<Option<String>>,
    pub blocked: Option<bool>,
    pub block_reason: Option<Option<String>>,
    pub updated_at: Option<String>,
    pub completed_at: Option<Option<String>>,
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
