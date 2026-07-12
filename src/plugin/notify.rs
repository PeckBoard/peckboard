//! Payload builders and async fire helpers for lifecycle notification hooks.
//!
//! Each `*_payload` builder is pure (no I/O, testable in isolation). Each
//! `fire_*` helper resolves any missing names from the DB, then calls
//! [`crate::plugin::manager::notify`] which spawns the actual plugin dispatch.

use serde_json::json;

use super::hooks::{CARD_STEP_AFTER_HOOK, SESSION_AGENT_ENDED_HOOK};
use super::manager::notify;

// ── card.step.after ─────────────────────────────────────────────────────────

pub fn card_step_after_payload(
    card_id: &str,
    card_title: &str,
    project_id: &str,
    project_name: &str,
    old_step: &str,
    new_step: &str,
) -> serde_json::Value {
    let terminal = matches!(new_step, "done" | "wont_do");
    json!({
        "card_id": card_id,
        "card_title": card_title,
        "project_id": project_id,
        "project_name": project_name,
        "old_step": old_step,
        "new_step": new_step,
        "terminal": terminal,
    })
}

pub async fn fire_card_step_after(
    db: &crate::db::Db,
    card_id: &str,
    card_title: &str,
    project_id: &str,
    old_step: &str,
    new_step: &str,
) {
    let project_name = match db.get_project(project_id).await {
        Ok(Some(p)) => p.name,
        _ => project_id.to_string(),
    };
    let payload = card_step_after_payload(
        card_id,
        card_title,
        project_id,
        &project_name,
        old_step,
        new_step,
    );
    notify(CARD_STEP_AFTER_HOOK, payload);
}

// ── session.agent.ended ──────────────────────────────────────────────────────

pub fn session_agent_ended_payload(
    session_id: &str,
    session_name: &str,
    is_worker: bool,
    outcome: &str,
    reason: Option<&str>,
) -> serde_json::Value {
    json!({
        "session_id": session_id,
        "session_name": session_name,
        "is_worker": is_worker,
        "outcome": outcome,
        "reason": reason,
    })
}

pub async fn fire_session_agent_ended(
    db: &crate::db::Db,
    session_id: &str,
    outcome: &str,
    reason: Option<&str>,
) {
    let (session_name, is_worker) = match db.get_session(session_id).await {
        Ok(Some(s)) => (s.name, s.is_worker),
        _ => (session_id.to_string(), false),
    };
    let payload =
        session_agent_ended_payload(session_id, &session_name, is_worker, outcome, reason);
    notify(SESSION_AGENT_ENDED_HOOK, payload);
}

// ── worker.blocked ───────────────────────────────────────────────────────────

pub fn worker_blocked_payload(
    card_id: &str,
    card_title: &str,
    project_id: &str,
    project_name: &str,
    reason: &str,
) -> serde_json::Value {
    json!({
        "card_id": card_id,
        "card_title": card_title,
        "project_id": project_id,
        "project_name": project_name,
        "reason": reason,
    })
}

// ── project.paused ───────────────────────────────────────────────────────────

pub fn project_paused_payload(
    project_id: &str,
    project_name: &str,
    reason: Option<&str>,
    source: &str,
) -> serde_json::Value {
    json!({
        "project_id": project_id,
        "project_name": project_name,
        "reason": reason,
        "source": source,
    })
}

// ── question.pending ─────────────────────────────────────────────────────────

pub fn question_pending_payload(
    session_id: &str,
    session_name: &str,
    preview: &str,
) -> serde_json::Value {
    json!({
        "session_id": session_id,
        "session_name": session_name,
        "preview": preview,
    })
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_step_after_payload_shape() {
        let v = card_step_after_payload("c1", "My Card", "p1", "My Project", "backlog", "done");
        assert_eq!(v["card_id"], "c1");
        assert_eq!(v["card_title"], "My Card");
        assert_eq!(v["project_id"], "p1");
        assert_eq!(v["project_name"], "My Project");
        assert_eq!(v["old_step"], "backlog");
        assert_eq!(v["new_step"], "done");
        assert_eq!(v["terminal"], true);
    }

    #[test]
    fn card_step_after_non_terminal() {
        let v = card_step_after_payload("c1", "T", "p1", "P", "backlog", "in_progress");
        assert_eq!(v["terminal"], false);
    }

    #[test]
    fn card_step_after_wont_do_is_terminal() {
        let v = card_step_after_payload("c1", "T", "p1", "P", "in_progress", "wont_do");
        assert_eq!(v["terminal"], true);
    }

    #[test]
    fn session_agent_ended_payload_shape() {
        let v = session_agent_ended_payload("s1", "My Session", true, "crashed", Some("oom"));
        assert_eq!(v["session_id"], "s1");
        assert_eq!(v["session_name"], "My Session");
        assert_eq!(v["is_worker"], true);
        assert_eq!(v["outcome"], "crashed");
        assert_eq!(v["reason"], "oom");
    }

    #[test]
    fn session_agent_ended_completed_no_reason() {
        let v = session_agent_ended_payload("s1", "S", false, "completed", None);
        assert_eq!(v["outcome"], "completed");
        assert!(v["reason"].is_null());
    }

    #[test]
    fn worker_blocked_payload_shape() {
        let v = worker_blocked_payload("c1", "Card", "p1", "Proj", "too many crashes");
        assert_eq!(v["card_id"], "c1");
        assert_eq!(v["reason"], "too many crashes");
    }

    #[test]
    fn project_paused_payload_shape() {
        let v = project_paused_payload("p1", "Proj", Some("crash"), "crash");
        assert_eq!(v["project_id"], "p1");
        assert_eq!(v["source"], "crash");
        assert_eq!(v["reason"], "crash");
    }

    #[test]
    fn project_paused_manual_no_reason() {
        let v = project_paused_payload("p1", "Proj", None, "manual");
        assert_eq!(v["source"], "manual");
        assert!(v["reason"].is_null());
    }

    #[test]
    fn question_pending_payload_shape() {
        let v = question_pending_payload("s1", "Chat", "What should I do?");
        assert_eq!(v["session_id"], "s1");
        assert_eq!(v["preview"], "What should I do?");
    }
}
