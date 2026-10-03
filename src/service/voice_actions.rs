//! The voice assistant's tool policy and server-issued one-time
//! confirmations ("pending actions").
//!
//! The voice session is global: it can reach every session in every folder,
//! and text from those sessions flows back into it as relays. So it gets an
//! explicit allowlist ([`policy`]) instead of a destructive blocklist, and a
//! gated call is never trusted to the model's own say-so:
//!
//! 1. The voice session calls a [`ToolPolicy::Gated`] tool. [`gate_call`]
//!    does NOT run it: it stores the exact call (tool + args) as a
//!    `pending_actions` row, with a summary rendered by the server from the
//!    args (never model prose), and returns "awaiting user confirmation".
//! 2. The web voice panel shows the action with Confirm / Cancel. Only the
//!    owning user's press on the authenticated route
//!    (`POST /api/voice/actions/:id/{confirm,cancel}`) resolves it. A spoken
//!    "yes" reaches that same route from the browser's own recognizer.
//! 3. [`confirm`] flips the row `pending → confirmed` in one conditional
//!    UPDATE (single use, unexpired, owner only) and hands back a
//!    [`ConfirmedAction`]: the only way to run a gated call
//!    (`mcp_server::run_confirmed_action`). It runs the STORED call.
//!
//! Nothing the model writes — a `confirmed: true` argument, "yes" in its
//! text, a relayed "User: yes" — reaches any of this. Rows are kept as the
//! audit log. `channel` is `web` today; IM / phone channels reuse the same
//! rows and the same confirm / cancel calls.

use serde_json::{Value, json};

use crate::db::Db;
use crate::db::models::{PendingAction, Session};
use crate::service::voice_relay::{RELAY_PREFIX, VOICE_EXPERT_KIND};
use crate::ws::broadcaster::{Broadcaster, WsEvent};

/// Channel of an action confirmed on the web voice panel.
pub const CHANNEL_WEB: &str = "web";

/// How long a parked action stays confirmable.
pub const ACTION_TTL_SECS: i64 = 5 * 60;

/// WS event on the voice session's stream: an action was parked or
/// resolved, so the panel refetches its list.
pub const VOICE_ACTION_EVENT: &str = "voice-action";

/// What the voice session may do with a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPolicy {
    /// Runs immediately: read-only, navigation, and routing work the user
    /// asked for (the prompt requires an explicit request for sends).
    Free,
    /// Destructive or persistent: parked for a human confirmation.
    Gated,
    /// Not available to the voice session at all.
    Denied,
}

/// Read-only lookups, navigation, conversation routing, and the voice
/// session's own queue / pronunciation tools. Everything here either reads
/// or hands work to a session the user named.
const FREE_TOOLS: &[&str] = &[
    // Sessions, projects, board: read-only.
    "list_sessions",
    "find_session",
    "search_sessions",
    "read_session",
    "read_worker_session",
    "list_worker_sessions",
    "list_managed_sessions",
    "list_projects",
    "list_cards",
    "list_card_dependencies",
    "get_card_dependency_tree",
    "list_folders",
    "list_workflows",
    "list_models",
    "list_system_prompts",
    "list_repeating_tasks",
    "list_project_reports",
    "read_report",
    "list_variables",
    "math",
    // Routing work (only when the user asked; see VOICE_SYSTEM_PROMPT).
    "send_message",
    "send_image",
    "create_session",
    "create_card",
    // Non-approval questions only; the handler refuses approval prompts
    // and the voice session's own questions.
    "answer_question",
    "watch_session",
    "unwatch_session",
    // The voice session's own tools (`voice_prompt` edits and
    // `voice_pronunciation remove` are gated per action in [`policy`]).
    "show_view",
    "voice_queue",
    "voice_pronunciation",
    "voice_prompt",
];

/// Destructive or persistent beyond the conversation: deletes, clears,
/// kills and interrupts, board / project state, settings, schedules,
/// prompts, variables, plugins, files, and commands (local, SSH, remote).
const GATED_TOOLS: &[&str] = &[
    "delete_card",
    "delete_project",
    "delete_repeating_task",
    "delete_variable",
    "clear_session",
    "terminate_agent",
    "interrupt_session",
    "update_card",
    "move_card_to_done",
    "move_card_to_wont_do",
    "create_project",
    "update_project",
    "pause_project",
    "resume_project",
    "create_folder",
    "set_session_system_prompt",
    "set_workflow_instructions",
    "set_variable",
    "create_repeating_task",
    "update_repeating_task",
    "upgrade_plugin",
    "app_install",
    "app_remove",
    "run_command",
    "write_file",
    "edit_file",
    "ssh_run",
    "ssh_run_many",
    "ssh_write_file",
    "ssh_edit_file",
    "ssh_host_add",
    "ssh_host_update",
    "ssh_host_remove",
    "remote_agent_run",
];

/// The voice session's policy for one call. `voice_prompt` edits and
/// `voice_pronunciation remove` are gated; their reads stay free.
pub fn policy(tool: &str, args: &Value) -> ToolPolicy {
    let action = args.get("action").and_then(|v| v.as_str());
    match tool {
        "voice_prompt" if !matches!(action, None | Some("get")) => ToolPolicy::Gated,
        "voice_pronunciation" if action == Some("remove") => ToolPolicy::Gated,
        _ if FREE_TOOLS.contains(&tool) => ToolPolicy::Free,
        _ if GATED_TOOLS.contains(&tool) => ToolPolicy::Gated,
        _ => ToolPolicy::Denied,
    }
}

/// Whether the voice session may call `tool` at all (free or gated) — the
/// name-only check `ToolGate` advertises and blocks by.
pub fn tool_allowed(tool: &str) -> bool {
    FREE_TOOLS.contains(&tool) || GATED_TOOLS.contains(&tool)
}

/// Outcome of [`gate_call`].
pub enum Gate {
    /// Not a voice session, or a free tool: dispatch as usual.
    Proceed,
    /// Parked; return this to the model instead of running the tool.
    Parked(Value),
}

fn is_voice(s: &Session) -> bool {
    s.expert_kind.as_deref() == Some(VOICE_EXPERT_KIND)
}

/// Fixed-width UTC timestamp, so `expires_at` compares correctly as text.
fn ts(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn now() -> String {
    ts(chrono::Utc::now())
}

/// The voice-session gate at dispatch. A no-op for every other session.
/// For the voice session: denied tools fail, free tools proceed, gated
/// tools are parked as a pending action. A model-sent `confirmed` argument
/// is dropped and means nothing.
pub async fn gate_call(
    db: &Db,
    broadcaster: &Broadcaster,
    caller_session_id: &str,
    tool: &str,
    args: &mut Value,
) -> anyhow::Result<Gate> {
    let session = match db.get_session(caller_session_id).await? {
        Some(s) if is_voice(&s) => s,
        _ => return Ok(Gate::Proceed),
    };
    if let Some(o) = args.as_object_mut() {
        o.remove("confirmed");
    }
    match policy(tool, args) {
        ToolPolicy::Free => Ok(Gate::Proceed),
        ToolPolicy::Denied => anyhow::bail!(
            "tool '{tool}' is not available to the voice assistant. Route that work to \
             another session with send_message."
        ),
        ToolPolicy::Gated => {
            let action = create(db, &session, CHANNEL_WEB, tool, args).await?;
            broadcast(broadcaster, &action);
            Ok(Gate::Parked(json!({
                "status": "awaiting_confirmation",
                "action_id": action.id,
                "summary": action.summary,
                "message": format!(
                    "Awaiting user confirmation, action {}. Nothing has run. The user sees a \
                     Confirm / Cancel card for: {}. Tell them in one sentence what is waiting; \
                     do not call the tool again.",
                    action.id, action.summary
                ),
            })))
        }
    }
}

/// Park one gated call for `session`'s owner.
pub async fn create(
    db: &Db,
    session: &Session,
    channel: &str,
    tool: &str,
    args: &Value,
) -> anyhow::Result<PendingAction> {
    let Some(user_id) = session.user_id.clone() else {
        anyhow::bail!(
            "tool '{tool}' needs the user's confirmation, but this session has no owner to ask"
        );
    };
    let created = chrono::Utc::now();
    let row = PendingAction {
        id: uuid::Uuid::new_v4().to_string(),
        user_id,
        session_id: session.id.clone(),
        channel: channel.to_string(),
        tool: tool.to_string(),
        args_json: args.to_string(),
        summary: summarize(db, tool, args).await,
        status: "pending".into(),
        created_at: ts(created),
        expires_at: ts(created + chrono::Duration::seconds(ACTION_TTL_SECS)),
        resolved_at: None,
        resolved_by: None,
        result_json: None,
    };
    db.insert_pending_action(row.clone()).await?;
    tracing::info!(
        action_id = %row.id, session_id = %row.session_id, user_id = %row.user_id,
        tool, summary = %row.summary, "pending action created"
    );
    Ok(row)
}

/// Tell browsers on the action's session stream to refetch.
pub fn broadcast(broadcaster: &Broadcaster, action: &PendingAction) {
    broadcaster.broadcast(WsEvent {
        event_type: VOICE_ACTION_EVENT.into(),
        session_id: action.session_id.clone(),
        data: json!({ "id": action.id, "status": action.status }),
    });
}

/// `user_id`'s open actions (expiring stale ones first).
pub async fn list_open(db: &Db, user_id: &str) -> anyhow::Result<Vec<PendingAction>> {
    let now = now();
    db.expire_pending_actions(&now).await?;
    db.list_open_pending_actions(user_id, &now).await
}

/// Why a confirm / cancel did nothing.
#[derive(Debug)]
pub enum ActionError {
    /// No such action for this user.
    NotFound,
    /// Already confirmed, cancelled, or expired.
    Resolved(String),
    Expired,
    Failed(anyhow::Error),
}

impl std::fmt::Display for ActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionError::NotFound => write!(f, "action not found"),
            ActionError::Resolved(s) => write!(f, "action already {s}"),
            ActionError::Expired => write!(f, "action expired; ask the assistant again"),
            ActionError::Failed(e) => write!(f, "{e}"),
        }
    }
}

impl From<anyhow::Error> for ActionError {
    fn from(e: anyhow::Error) -> Self {
        ActionError::Failed(e)
    }
}

// Proof token: bearer won the single-use `pending → confirmed` transition
// of a pending action for its owner via `confirm`, i.e. a human pressed
// Confirm on an authenticated route. See `routes::voice::confirm_action`
// for an example. It carries the STORED call; fields are private, so a
// gated tool can only run with exactly what was parked.
pub struct ConfirmedAction {
    row: PendingAction,
    args: Value,
}

impl ConfirmedAction {
    pub fn id(&self) -> &str {
        &self.row.id
    }
    pub fn tool(&self) -> &str {
        &self.row.tool
    }
    pub fn args(&self) -> &Value {
        &self.args
    }
    pub fn session_id(&self) -> &str {
        &self.row.session_id
    }
    pub fn summary(&self) -> &str {
        &self.row.summary
    }
    pub fn row(&self) -> &PendingAction {
        &self.row
    }
}

/// The user pressed Confirm: consume the action (single use, unexpired,
/// owner only) and return the proof token that runs it.
pub async fn confirm(db: &Db, id: &str, user_id: &str) -> Result<ConfirmedAction, ActionError> {
    let now = now();
    if !db
        .resolve_pending_action(id, user_id, "confirmed", &now)
        .await?
    {
        return Err(why_not(db, id, user_id, &now).await);
    }
    let row = db
        .get_pending_action(id)
        .await?
        .ok_or(ActionError::NotFound)?;
    let args = serde_json::from_str(&row.args_json)
        .map_err(|e| ActionError::Failed(anyhow::anyhow!("stored args unreadable: {e}")))?;
    tracing::info!(action_id = %row.id, user_id, tool = %row.tool, "pending action confirmed");
    Ok(ConfirmedAction { row, args })
}

/// The user pressed Cancel: the action can never run.
pub async fn cancel(db: &Db, id: &str, user_id: &str) -> Result<PendingAction, ActionError> {
    let now = now();
    if !db
        .resolve_pending_action(id, user_id, "cancelled", &now)
        .await?
    {
        return Err(why_not(db, id, user_id, &now).await);
    }
    tracing::info!(action_id = %id, user_id, "pending action cancelled");
    db.get_pending_action(id)
        .await?
        .ok_or(ActionError::NotFound)
}

async fn why_not(db: &Db, id: &str, user_id: &str, now: &str) -> ActionError {
    match db.get_pending_action(id).await {
        Ok(Some(row)) if row.user_id == user_id => {
            if row.status != "pending" {
                ActionError::Resolved(row.status)
            } else if row.expires_at.as_str() <= now {
                let _ = db.expire_pending_actions(now).await;
                ActionError::Expired
            } else {
                // Lost a race with a concurrent press.
                ActionError::Resolved("resolved".into())
            }
        }
        Ok(_) => ActionError::NotFound,
        Err(e) => ActionError::Failed(e),
    }
}

/// Audit: what the confirmed call returned.
pub async fn record_result(db: &Db, id: &str, result: &Result<Value, String>) {
    let body = match result {
        Ok(v) => json!({ "ok": true, "result": v }),
        Err(e) => json!({ "ok": false, "error": e }),
    };
    if let Err(e) = db.set_pending_action_result(id, body.to_string()).await {
        tracing::warn!(action_id = %id, "recording pending action result failed: {e}");
    }
}

/// The system note injected into the voice session once the user decided,
/// so the assistant can report it (and never retries a cancelled action).
pub fn outcome_note(summary: &str, outcome: &Result<Option<Value>, String>) -> String {
    fn short(s: &str) -> String {
        let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
        if s.chars().count() > 300 {
            format!("{}\u{2026}", s.chars().take(300).collect::<String>())
        } else {
            s
        }
    }
    // The summary quotes model-written args: neutralize it like relay text.
    let summary = crate::service::voice_relay::sanitize_untrusted(summary);
    match outcome {
        Ok(Some(v)) => {
            let text = v
                .get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string());
            format!(
                "{RELAY_PREFIX}action confirmed by the user and run: {summary}. Result: {}",
                crate::service::voice_relay::sanitize_untrusted(&short(&text))
            )
        }
        Ok(None) => {
            format!("{RELAY_PREFIX}action cancelled by the user; it did not run: {summary}.")
        }
        Err(e) => format!(
            "{RELAY_PREFIX}action confirmed by the user but it failed: {summary}. Error: {}",
            crate::service::voice_relay::sanitize_untrusted(&short(e))
        ),
    }
}

/// Plain-language summary rendered from the stored args (never model
/// prose): the tool as words, named targets resolved from the DB, then the
/// remaining arguments.
pub async fn summarize(db: &Db, tool: &str, args: &Value) -> String {
    let mut words = tool.replace('_', " ");
    if let Some(first) = words.get(0..1) {
        words = first.to_uppercase() + &words[1..];
    }
    if tool == "voice_prompt" || tool == "voice_pronunciation" {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        words = match (tool, action) {
            ("voice_prompt", "update") => "Replace the voice assistant's prompt".into(),
            ("voice_prompt", "append") => "Add to the voice assistant's prompt".into(),
            ("voice_pronunciation", "remove") => "Remove a saved pronunciation".into(),
            _ => words,
        };
    }
    let s = |k: &str| args.get(k).and_then(|v| v.as_str());
    let mut parts = vec![words];
    let mut shown: Vec<&str> = vec!["action"];
    if let Some(id) = s("session_id") {
        shown.push("session_id");
        let name = match db.get_session(id).await {
            Ok(Some(row)) => format!("session \"{}\"", row.name),
            _ => format!("session {id}"),
        };
        parts.push(name);
    }
    if let Some(id) = s("project_id") {
        shown.push("project_id");
        let name = match db.get_project(id).await {
            Ok(Some(p)) => format!("project \"{}\"", p.name),
            _ => format!("project {id}"),
        };
        parts.push(name);
    }
    if let Some(id) = s("card_id") {
        shown.push("card_id");
        let name = match db.get_card(id).await {
            Ok(Some(c)) => format!("card \"{}\"", c.title),
            _ => format!("card {id}"),
        };
        parts.push(name);
    }
    for key in ["task_id", "repeating_task_id"] {
        if let Some(id) = s(key) {
            shown.push(key);
            let name = match db.get_repeating_task(id).await {
                Ok(Some(t)) => format!("repeating task \"{}\"", t.name),
                _ => format!("repeating task {id}"),
            };
            parts.push(name);
        }
    }
    let mut rest = Vec::new();
    if let Some(obj) = args.as_object() {
        for (k, v) in obj {
            if shown.contains(&k.as_str()) {
                continue;
            }
            let v = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            rest.push(format!("{k}: {v}"));
        }
    }
    let mut out = parts.join(", ");
    if !rest.is_empty() {
        out.push_str(" \u{2014} ");
        out.push_str(&rest.join("; "));
    }
    let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.chars().count() > 600 {
        format!("{}\u{2026}", out.chars().take(600).collect::<String>())
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{NewFolder, NewSession};

    async fn setup() -> Db {
        let db = Db::in_memory().unwrap();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "f".into(),
            path: "/tmp".into(),
            created_at: "now".into(),
        })
        .await
        .unwrap();
        for (id, kind) in [("voice", Some(VOICE_EXPERT_KIND)), ("chat", None)] {
            db.create_session(NewSession {
                id: id.into(),
                name: id.into(),
                folder_id: "f1".into(),
                created_at: "now".into(),
                last_activity: "now".into(),
                is_expert: kind.is_some(),
                expert_kind: kind.map(str::to_string),
                user_id: Some("u1".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        db
    }

    #[test]
    fn voice_policy_is_an_allowlist() {
        let none = json!({});
        assert_eq!(policy("list_sessions", &none), ToolPolicy::Free);
        assert_eq!(policy("send_message", &none), ToolPolicy::Free);
        for t in [
            "delete_project",
            "clear_session",
            "terminate_agent",
            "interrupt_session",
            "set_session_system_prompt",
            "create_repeating_task",
            "update_repeating_task",
            "upgrade_plugin",
            "app_install",
            "ssh_run",
            "run_command",
            "write_file",
            "edit_file",
            "set_variable",
        ] {
            assert_eq!(policy(t, &none), ToolPolicy::Gated, "{t}");
        }
        // Unknown and unlisted tools are denied, not merely gated.
        for t in [
            "browser_open",
            "spawn_subagent",
            "fetch_url",
            "read_file",
            "git",
            "made_up",
        ] {
            assert_eq!(policy(t, &none), ToolPolicy::Denied, "{t}");
            assert!(!tool_allowed(t));
        }
        assert_eq!(
            policy("voice_prompt", &json!({"action": "get"})),
            ToolPolicy::Free
        );
        assert_eq!(policy("voice_prompt", &none), ToolPolicy::Free);
        assert_eq!(
            policy("voice_prompt", &json!({"action": "append"})),
            ToolPolicy::Gated
        );
        assert_eq!(
            policy("voice_pronunciation", &json!({"action": "remove"})),
            ToolPolicy::Gated
        );
        assert_eq!(
            policy("voice_pronunciation", &json!({"action": "add"})),
            ToolPolicy::Free
        );
    }

    #[tokio::test]
    async fn gated_voice_call_parks_and_confirms_exactly_once() {
        let db = setup().await;
        let bc = Broadcaster::new();

        // Non-voice callers are untouched.
        let mut args = json!({"project_id": "p"});
        assert!(matches!(
            gate_call(&db, &bc, "chat", "delete_project", &mut args)
                .await
                .unwrap(),
            Gate::Proceed
        ));
        // Voice: denied tools fail, free tools proceed.
        assert!(
            gate_call(&db, &bc, "voice", "browser_open", &mut json!({}))
                .await
                .is_err()
        );
        assert!(matches!(
            gate_call(&db, &bc, "voice", "list_sessions", &mut json!({}))
                .await
                .unwrap(),
            Gate::Proceed
        ));

        // A model-asserted `confirmed: true` changes nothing: still parked,
        // and the flag is not stored.
        let mut args = json!({"session_id": "chat", "confirmed": true});
        let Gate::Parked(out) = gate_call(&db, &bc, "voice", "clear_session", &mut args)
            .await
            .unwrap()
        else {
            panic!("expected parked");
        };
        assert_eq!(out["status"], "awaiting_confirmation");
        let id = out["action_id"].as_str().unwrap().to_string();
        let open = list_open(&db, "u1").await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].summary, "Clear session, session \"chat\"");
        assert_eq!(open[0].args_json, json!({"session_id": "chat"}).to_string());

        // Another user can't confirm it.
        assert!(matches!(
            confirm(&db, &id, "u2").await,
            Err(ActionError::NotFound)
        ));
        // The owner can — once.
        let token = confirm(&db, &id, "u1").await.unwrap();
        assert_eq!(token.tool(), "clear_session");
        assert_eq!(token.args(), &json!({"session_id": "chat"}));
        assert!(matches!(
            confirm(&db, &id, "u1").await,
            Err(ActionError::Resolved(_))
        ));
        assert!(matches!(
            cancel(&db, &id, "u1").await,
            Err(ActionError::Resolved(_))
        ));
        assert!(list_open(&db, "u1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancelled_and_expired_actions_never_confirm() {
        let db = setup().await;
        let voice = db.get_session("voice").await.unwrap().unwrap();
        let a = create(
            &db,
            &voice,
            CHANNEL_WEB,
            "set_variable",
            &json!({"name": "x"}),
        )
        .await
        .unwrap();
        cancel(&db, &a.id, "u1").await.unwrap();
        assert!(matches!(
            confirm(&db, &a.id, "u1").await,
            Err(ActionError::Resolved(s)) if s == "cancelled"
        ));

        let mut b = create(
            &db,
            &voice,
            CHANNEL_WEB,
            "set_variable",
            &json!({"name": "y"}),
        )
        .await
        .unwrap();
        // Backdate: insert an already-expired twin.
        b.id = "expired".into();
        b.expires_at = ts(chrono::Utc::now() - chrono::Duration::seconds(1));
        db.insert_pending_action(b).await.unwrap();
        assert!(matches!(
            confirm(&db, "expired", "u1").await,
            Err(ActionError::Expired)
        ));
        let row = db.get_pending_action("expired").await.unwrap().unwrap();
        assert_eq!(row.status, "expired");
    }
}
