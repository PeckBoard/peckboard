//! `reattach_worker`: put a worker session back on a card it was detached
//! from (a watchdog sweep, a no-progress turn end, a lost background task),
//! optionally lifting the block that detachment left behind. The MCP
//! handler (`handlers/reattach.rs`) validates scope and emits a marker; the
//! `mcp` route runs [`reattach_worker`] here, where the session manager's
//! per-session lock is available.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::db::models::UpdateSession;
use crate::service::mcp_server::ScopedProjectId;
use crate::state::AppState;

use super::orchestrator;

/// Reattach `session_id` to `card_id` inside the already scope-checked
/// `project`. Everything is re-validated under the session lock, so a
/// concurrent spawn / completion can't interleave between check and claim.
pub async fn reattach_worker(
    state: &Arc<AppState>,
    project: &ScopedProjectId,
    card_id: &str,
    session_id: &str,
    unblock: bool,
    reason: &str,
) -> anyhow::Result<Value> {
    let lock = state.session_manager.lock_session(session_id).await;
    let db = &state.db;

    let card = db
        .get_card(card_id)
        .await?
        .filter(|c| c.project_id == project.as_str())
        .ok_or_else(|| anyhow::anyhow!("card not found: {card_id}"))?;
    if card.step == "done" || card.step == "wont_do" {
        anyhow::bail!("card {card_id} is {}; nothing to reattach", card.step);
    }
    if let Some(holder) = card.worker_session_id.as_deref()
        && holder != session_id
    {
        anyhow::bail!("card {card_id} is already assigned to worker session {holder}");
    }

    let session = db
        .get_session(lock.session_id())
        .await?
        .ok_or_else(|| anyhow::anyhow!("session not found: {session_id}"))?;
    if !session.is_worker {
        anyhow::bail!("session {session_id} is not a worker session");
    }
    if session.project_id.as_deref() != Some(project.as_str()) {
        anyhow::bail!("session {session_id} is not a worker of this card's project");
    }
    if let Some(other) = session.card_id.as_deref()
        && other != card_id
    {
        anyhow::bail!("session {session_id} belongs to another card ({other})");
    }
    for other in db.list_worker_sessions_by_project(project.as_str()).await? {
        if other.id != session_id
            && other.card_id.as_deref() == Some(card_id)
            && state.session_manager.is_running(&other.id).await
        {
            anyhow::bail!(
                "worker session {} is currently running on card {card_id}; stop it first",
                other.id
            );
        }
    }

    let now = chrono::Utc::now().to_rfc3339();
    if card.worker_session_id.is_none()
        && !db
            .claim_card_for_worker(card_id, session_id, None, &now)
            .await?
    {
        anyhow::bail!("card {card_id} was claimed by another worker meanwhile");
    }
    db.update_session(
        session_id,
        UpdateSession {
            card_id: Some(Some(card_id.to_string())),
            worker_step: Some(Some(card.step.clone())),
            ..Default::default()
        },
    )
    .await?;

    let mut unblocked = false;
    if unblock && card.blocked {
        if card.block_reason.as_deref() == Some(crate::service::questions::ASK_USER_BLOCK_REASON) {
            // Only lifts once no question is left pending on the session.
            crate::service::questions::clear_question_block(db, &state.broadcaster, session_id)
                .await;
        } else {
            db.update_card(
                card_id,
                crate::db::models::UpdateCard {
                    blocked: Some(false),
                    block_reason: Some(None),
                    updated_at: Some(now),
                    ..Default::default()
                },
            )
            .await?;
            // Reset the crash / no-progress budgets for the fresh attempt.
            orchestrator::mark_card_unblocked(db, card_id).await?;
        }
        unblocked = !db.get_card(card_id).await?.is_some_and(|c| c.blocked);
    }
    drop(lock);

    tracing::info!(
        card_id = %card_id,
        session_id = %session_id,
        unblocked,
        reason = %reason,
        "reattach_worker: worker reattached to card"
    );
    orchestrator::broadcast_card_update(state, card_id, project.as_str());

    let still_blocked = db.get_card(card_id).await?.is_some_and(|c| c.blocked);
    Ok(json!({
        "status": "ok",
        "card_id": card_id,
        "session_id": session_id,
        "blocked": still_blocked,
        "message": if still_blocked {
            "Worker reattached; the card is still blocked (a question is still pending, or unblock was false)."
        } else {
            "Worker reattached and the card is unblocked."
        },
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::db::models::{NewCard, NewFolder, NewProject, NewSession, UpdateCard};
    use crate::service::mcp_server::ToolCallContext;

    async fn seed(state: &crate::state::AppState) {
        let db = &state.db;
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_project(NewProject {
            id: "p1".into(),
            name: "P".into(),
            context: "".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
        })
        .await
        .unwrap();
        db.create_card(NewCard {
            id: "c1".into(),
            project_id: "p1".into(),
            title: "Review".into(),
            description: "".into(),
            step: "review".into(),
            priority: 1,
            workflow: "task".into(),
            model: None,
            effort: None,
            blocked: true,
            block_reason: Some("ended 4 turns in a row without advancing".into()),
            created_at: ts.clone(),
            updated_at: ts.clone(),
            system_prompt_name: None,
        })
        .await
        .unwrap();
        for (id, is_worker) in [("ws1", true), ("chat", false)] {
            db.create_session(NewSession {
                id: id.into(),
                name: id.into(),
                folder_id: "f1".into(),
                is_worker,
                project_id: is_worker.then(|| "p1".into()),
                created_at: ts.clone(),
                last_activity: ts.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        }
    }

    fn ctx(state: &crate::state::AppState) -> ToolCallContext {
        ToolCallContext {
            session_id: "chat".into(),
            project_id: None,
            card_id: None,
            folder_id: "f1".into(),
            db: Arc::new(state.db.clone()),
            broadcaster: state.broadcaster.clone(),
            provider_registry: None,
            data_dir: None,
            device_registry: None,
            background: None,
        }
    }

    #[tokio::test]
    async fn reattach_claims_card_and_unblocks() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::auth::middleware::tests::test_state(dir.path());
        seed(&state).await;
        let scope = ctx(&state).scope_card("c1").await.unwrap();

        super::reattach_worker(&state, &scope, "c1", "ws1", true, "detached while testing")
            .await
            .unwrap();

        let card = state.db.get_card("c1").await.unwrap().unwrap();
        assert_eq!(card.worker_session_id.as_deref(), Some("ws1"));
        assert!(!card.blocked);
        let session = state.db.get_session("ws1").await.unwrap().unwrap();
        assert_eq!(session.card_id.as_deref(), Some("c1"));
        assert_eq!(session.worker_step.as_deref(), Some("review"));
        // The no-progress budget restarts from the reattach.
        let events = state.db.list_events_by_session("ws1", None).await.unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.kind == crate::worker::pipeline::PAUSE_CLEARED_KIND)
        );
    }

    #[tokio::test]
    async fn reattach_refuses_card_held_by_another_session() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::auth::middleware::tests::test_state(dir.path());
        seed(&state).await;
        state
            .db
            .update_card(
                "c1",
                UpdateCard {
                    worker_session_id: Some(Some("chat".into())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let scope = ctx(&state).scope_card("c1").await.unwrap();

        let err = super::reattach_worker(&state, &scope, "c1", "ws1", true, "r")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already assigned"), "{err}");
        let card = state.db.get_card("c1").await.unwrap().unwrap();
        assert!(card.blocked, "a refused reattach changes nothing");
    }
}
