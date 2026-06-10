//! Integration tests for the synchronous card-transition flow driven by
//! the MCP terminal tools (`finish_card`, `complete_step`, `wont_do_card`).
//!
//! Before the synchronous path existed, these tools only appended a
//! `*-requested` event and waited for the worker process to exit so
//! `handle_worker_done` could derive the intent. The Claude CLI keeps
//! its child alive between turns, though, so the card sat in its
//! pre-call step until the 30-minute idle reaper killed the process —
//! making the kanban look like the worker had silently ignored the
//! tool. These tests pin the new contract: the card transitions *during*
//! the tool call, and a later `handle_worker_done` for the same session
//! is a no-op rather than a re-application of the intent.
//!
//! All state is built on an in-memory DB; no `claude` CLI is involved.
//! The MCP handlers' worker-cancel hop is exercised with
//! `provider_registry = None`, which makes the cancel a no-op — the
//! tests are about state, not subprocess management.

use std::sync::Arc;

use peckboard::auth::rate_limit::RateLimiter;
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewCard, NewFolder, NewProject, NewSession, UpdateCard};
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::mock::register_mock_provider;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::service::mcp_server::{McpTokenRegistry, McpToolRegistry, ToolCallContext};
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::worker::orchestrator::handle_worker_done;
use peckboard::ws::broadcaster::Broadcaster;

async fn build_state() -> Arc<AppState> {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    std::mem::forget(tmp);

    let registry = Arc::new(ProviderRegistry::new());
    register_mock_provider(&registry).await;

    let db = Db::in_memory().unwrap();
    let plugins = Arc::new(PluginManager::new(&data_dir));
    let session_manager = SessionManager::new(registry.clone()).with_plugins(plugins.clone());

    Arc::new(AppState {
        config: Config {
            port: 0,
            https_port: 0,
            host: "127.0.0.1".into(),
            data_dir: data_dir.clone(),
            mdns: false,
        },
        db,
        plugins,
        jwt_secret: vec![0u8; 32],
        login_limiter: RateLimiter::new(100),
        password_change_limiter: RateLimiter::new(100),
        broadcaster: Broadcaster::new(),
        provider_registry: registry,
        session_manager,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        mcp_tokens: McpTokenRegistry::new(),
        push_service: PushService::new(&data_dir),
        pm_authorizations: Default::default(),
    })
}

/// Seed a folder + project + a single card at `step`, plus a worker session
/// assigned to that card. Returns `(card_id, session_id)`.
async fn seed_card_with_worker(
    state: &Arc<AppState>,
    step: &str,
    card_workflow: &str,
    project_default: &str,
) -> (String, String) {
    let ts = chrono::Utc::now().to_rfc3339();
    let card_id = uuid::Uuid::new_v4().to_string();
    let session_id = uuid::Uuid::new_v4().to_string();

    state
        .db
        .create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp".into(),
            created_at: ts.clone(),
        })
        .await
        .ok();

    state
        .db
        .create_project(NewProject {
            id: "p1".into(),
            name: "P".into(),
            context: "ctx".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: project_default.to_string(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
        })
        .await
        .ok();

    state
        .db
        .create_card(NewCard {
            id: card_id.clone(),
            project_id: "p1".into(),
            title: "card".into(),
            description: "desc".into(),
            step: step.into(),
            priority: 1,
            workflow: card_workflow.to_string(),
            model: None,
            effort: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();

    state
        .db
        .create_session(NewSession {
            id: session_id.clone(),
            name: "worker".into(),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker: true,
            project_id: Some("p1".into()),
            card_id: Some(card_id.clone()),
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();

    // Mirror production: spawn_worker_for_card sets worker_session_id on
    // the card row when it dispatches the run.
    state
        .db
        .update_card(
            &card_id,
            UpdateCard {
                worker_session_id: Some(Some(session_id.clone())),
                last_worker_session_id: Some(Some(session_id.clone())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    (card_id, session_id)
}

fn ctx_for_card(state: &Arc<AppState>, session_id: &str, card_id: &str) -> ToolCallContext {
    ToolCallContext {
        session_id: session_id.to_string(),
        project_id: Some("p1".into()),
        card_id: Some(card_id.to_string()),
        db: Arc::new(state.db.clone()),
        broadcaster: state.broadcaster.clone(),
        // `provider_registry: None` makes the cancel hop in the handlers
        // a no-op. The transition itself doesn't depend on the registry,
        // and the cancel path is exercised by the live provider tests.
        provider_registry: None,
        expert_dispatcher: None,
        data_dir: None,
        pm_authorizations: Default::default(),
    }
}

// ── finish_card ───────────────────────────────────────────────────────

#[tokio::test]
async fn finish_card_transitions_synchronously() {
    // The MCP call alone must drive the card to `done` — no
    // `handle_worker_done` round trip required. This is the regression
    // bug the user hit: the agent calls `finish_card`, the call returns
    // ok, but the kanban shows the card still in `in_progress` because
    // the worker process was waiting on idle.
    let state = build_state().await;
    let (card_id, session_id) = seed_card_with_worker(
        &state,
        "in_progress",
        "deep-develop-software",
        "deep-develop-software",
    )
    .await;
    let registry = McpToolRegistry::new();
    let ctx = ctx_for_card(&state, &session_id, &card_id);

    let result = registry
        .handle_tool_call(
            "finish_card",
            serde_json::json!({ "summary": "shipped it" }),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "ok");
    assert_eq!(result["to"], "done");
    assert_eq!(result["from"], "in_progress");

    let card = state.db.get_card(&card_id).await.unwrap().unwrap();
    assert_eq!(card.step, "done");
    assert!(
        card.worker_session_id.is_none(),
        "worker slot must be freed"
    );
    assert_eq!(
        card.last_worker_session_id.as_deref(),
        Some(session_id.as_str())
    );
    assert_eq!(card.handoff_context.as_deref(), Some("shipped it"));
    assert!(
        card.completed_at.is_some(),
        "transitioning to done must stamp completed_at"
    );
}

#[tokio::test]
async fn finish_card_lands_on_done_from_any_intermediate_step() {
    // `finish_card` is the "whole card is done, even though the workflow
    // has more steps" escape hatch. From any non-terminal step it must
    // reach `done` so dependent cards unblock.
    for start in ["backlog", "in_progress", "review"] {
        let state = build_state().await;
        let (card_id, session_id) = seed_card_with_worker(
            &state,
            start,
            "deep-develop-software",
            "deep-develop-software",
        )
        .await;
        let registry = McpToolRegistry::new();
        let ctx = ctx_for_card(&state, &session_id, &card_id);

        registry
            .handle_tool_call("finish_card", serde_json::json!({}), &ctx)
            .await
            .unwrap();

        let card = state.db.get_card(&card_id).await.unwrap().unwrap();
        assert_eq!(card.step, "done", "from {start} must reach done");
    }
}

#[tokio::test]
async fn finish_card_writes_audit_and_boundary_events() {
    let state = build_state().await;
    let (card_id, session_id) = seed_card_with_worker(&state, "in_progress", "task", "task").await;
    let registry = McpToolRegistry::new();
    let ctx = ctx_for_card(&state, &session_id, &card_id);

    registry
        .handle_tool_call(
            "finish_card",
            serde_json::json!({ "summary": "done" }),
            &ctx,
        )
        .await
        .unwrap();

    let events = state
        .db
        .list_events_by_session(&session_id, None)
        .await
        .unwrap();
    // The `finish-requested` event lands first (audit), `step-change`
    // second (the derive-worker-intent boundary). Order matters: if
    // step-change came first and was the rposition target, the
    // *-requested AFTER would be inside the window and re-fire.
    let finish_idx = events
        .iter()
        .position(|e| e.kind == "finish-requested")
        .expect("finish-requested must be appended");
    let change_idx = events
        .iter()
        .position(|e| e.kind == "step-change")
        .expect("step-change must be appended");
    assert!(
        finish_idx < change_idx,
        "audit event must precede the step-change boundary",
    );
}

#[tokio::test]
async fn handle_worker_done_after_finish_is_idempotent() {
    // The whole point of the step-change boundary: if the worker
    // process eventually exits and triggers `handle_worker_done`, the
    // synchronously-transitioned card must NOT be touched again — no
    // doubled handoff_context, no reverted worker_session_id, nothing.
    let state = build_state().await;
    let (card_id, session_id) = seed_card_with_worker(&state, "in_progress", "task", "task").await;
    let registry = McpToolRegistry::new();
    let ctx = ctx_for_card(&state, &session_id, &card_id);

    registry
        .handle_tool_call("finish_card", serde_json::json!({ "summary": "v1" }), &ctx)
        .await
        .unwrap();

    let before = state.db.get_card(&card_id).await.unwrap().unwrap();

    // Pretend the worker process eventually exited gracefully and the
    // completion listener ran the worker-done handler.
    handle_worker_done(&state, &session_id).await;

    let after = state.db.get_card(&card_id).await.unwrap().unwrap();
    assert_eq!(after.step, before.step);
    assert_eq!(after.handoff_context, before.handoff_context);
    assert_eq!(after.completed_at, before.completed_at);
    assert_eq!(after.worker_session_id, before.worker_session_id);
}

#[tokio::test]
async fn finish_card_refuses_to_overturn_wont_do() {
    // A wont_do card is an explicit "we're not doing this". The
    // surviving agent calling finish_card shouldn't be able to silently
    // promote it to done.
    let state = build_state().await;
    let (card_id, session_id) = seed_card_with_worker(&state, "in_progress", "task", "task").await;
    state
        .db
        .update_card(
            &card_id,
            UpdateCard {
                step: Some("wont_do".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let registry = McpToolRegistry::new();
    let ctx = ctx_for_card(&state, &session_id, &card_id);

    let err = registry
        .handle_tool_call("finish_card", serde_json::json!({}), &ctx)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("wont_do"),
        "expected wont_do refusal, got: {err}"
    );

    let card = state.db.get_card(&card_id).await.unwrap().unwrap();
    assert_eq!(card.step, "wont_do");
}

// ── complete_step ─────────────────────────────────────────────────────

#[tokio::test]
async fn complete_step_advances_synchronously() {
    // For a multi-step workflow, complete_step must move the card to the
    // next step IMMEDIATELY so the orchestrator's next tick spawns a
    // fresh worker for the new step — that's how context gets cleared
    // between steps in deep-develop-software (in_progress → review).
    let state = build_state().await;
    let (card_id, session_id) = seed_card_with_worker(
        &state,
        "in_progress",
        "deep-develop-software",
        "deep-develop-software",
    )
    .await;
    let registry = McpToolRegistry::new();
    let ctx = ctx_for_card(&state, &session_id, &card_id);

    let result = registry
        .handle_tool_call(
            "complete_step",
            serde_json::json!({ "handoff_context": "branch feat/x" }),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "ok");
    assert_eq!(result["to"], "review");
    assert_eq!(result["from"], "in_progress");

    let card = state.db.get_card(&card_id).await.unwrap().unwrap();
    assert_eq!(card.step, "review");
    // Slot freed so the next orchestrator tick can spawn a fresh
    // worker for the new step with a clean context window.
    assert!(card.worker_session_id.is_none());
    assert_eq!(
        card.last_worker_session_id.as_deref(),
        Some(session_id.as_str())
    );
    assert_eq!(card.handoff_context.as_deref(), Some("branch feat/x"));
    // `review` is non-terminal — completed_at must NOT be stamped here.
    assert!(
        card.completed_at.is_none(),
        "completed_at is only for the done terminal step"
    );
}

#[tokio::test]
async fn complete_step_handle_worker_done_no_double_advance() {
    // THE critical regression: after the MCP handler synchronously
    // advances by one step, the worker process's eventual graceful exit
    // triggers `handle_worker_done`, which used to walk the events and
    // see `complete-step-requested` → advance AGAIN. The step-change
    // sentinel the new handler appends is the boundary that stops that.
    //
    // deep-develop-software: backlog → in_progress → review → done. A
    // double-advance from in_progress would land the card on `done` and
    // unblock dependents prematurely.
    let state = build_state().await;
    let (card_id, session_id) = seed_card_with_worker(
        &state,
        "in_progress",
        "deep-develop-software",
        "deep-develop-software",
    )
    .await;
    let registry = McpToolRegistry::new();
    let ctx = ctx_for_card(&state, &session_id, &card_id);

    registry
        .handle_tool_call(
            "complete_step",
            serde_json::json!({ "handoff_context": "ready for review" }),
            &ctx,
        )
        .await
        .unwrap();
    let after_mcp = state.db.get_card(&card_id).await.unwrap().unwrap();
    assert_eq!(after_mcp.step, "review");

    handle_worker_done(&state, &session_id).await;

    let after_done = state.db.get_card(&card_id).await.unwrap().unwrap();
    assert_eq!(
        after_done.step, "review",
        "handle_worker_done must not re-advance past the step-change boundary"
    );
    assert_eq!(
        after_done.handoff_context.as_deref(),
        Some("ready for review")
    );
}

#[tokio::test]
async fn complete_step_writes_audit_then_boundary() {
    let state = build_state().await;
    let (card_id, session_id) = seed_card_with_worker(
        &state,
        "in_progress",
        "deep-develop-software",
        "deep-develop-software",
    )
    .await;
    let registry = McpToolRegistry::new();
    let ctx = ctx_for_card(&state, &session_id, &card_id);

    registry
        .handle_tool_call(
            "complete_step",
            serde_json::json!({ "handoff_context": "x" }),
            &ctx,
        )
        .await
        .unwrap();

    let events = state
        .db
        .list_events_by_session(&session_id, None)
        .await
        .unwrap();
    let req_idx = events
        .iter()
        .position(|e| e.kind == "complete-step-requested")
        .expect("complete-step-requested must be appended");
    let change_idx = events
        .iter()
        .position(|e| e.kind == "step-change")
        .expect("step-change must be appended");
    assert!(req_idx < change_idx);

    // step-change payload should describe the transition the handler
    // actually performed (in_progress → review for deep-develop-software).
    let change_data: serde_json::Value = serde_json::from_str(&events[change_idx].data).unwrap();
    assert_eq!(change_data["from"], "in_progress");
    assert_eq!(change_data["to"], "review");
}

#[tokio::test]
async fn complete_step_at_last_non_terminal_step_lands_on_done() {
    // `review` is the last non-terminal step of deep-develop-software,
    // so advancing from it must reach `done`. The handler falls back to
    // "done" if `find_next_step` returns None — covers the "review →
    // done" jump.
    let state = build_state().await;
    let (card_id, session_id) = seed_card_with_worker(
        &state,
        "review",
        "deep-develop-software",
        "deep-develop-software",
    )
    .await;
    let registry = McpToolRegistry::new();
    let ctx = ctx_for_card(&state, &session_id, &card_id);

    registry
        .handle_tool_call(
            "complete_step",
            serde_json::json!({ "handoff_context": "lgtm" }),
            &ctx,
        )
        .await
        .unwrap();
    let card = state.db.get_card(&card_id).await.unwrap().unwrap();
    assert_eq!(card.step, "done");
    assert!(card.completed_at.is_some());
}

#[tokio::test]
async fn complete_step_refuses_on_terminal_card() {
    // The agent racing a manual user "move to done": the MCP call must
    // not silently advance past the terminal state.
    let state = build_state().await;
    let (card_id, session_id) = seed_card_with_worker(&state, "in_progress", "task", "task").await;
    state
        .db
        .update_card(
            &card_id,
            UpdateCard {
                step: Some("done".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let registry = McpToolRegistry::new();
    let ctx = ctx_for_card(&state, &session_id, &card_id);

    let err = registry
        .handle_tool_call("complete_step", serde_json::json!({}), &ctx)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("terminal"),
        "expected terminal-state refusal, got: {err}"
    );

    let card = state.db.get_card(&card_id).await.unwrap().unwrap();
    assert_eq!(card.step, "done");
}

// ── wont_do_card ─────────────────────────────────────────────────────

#[tokio::test]
async fn wont_do_transitions_synchronously() {
    let state = build_state().await;
    let (card_id, session_id) = seed_card_with_worker(&state, "in_progress", "task", "task").await;
    let registry = McpToolRegistry::new();
    let ctx = ctx_for_card(&state, &session_id, &card_id);

    let result = registry
        .handle_tool_call(
            "wont_do_card",
            serde_json::json!({ "reason": "scope changed" }),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "ok");
    assert_eq!(result["to"], "wont_do");

    let card = state.db.get_card(&card_id).await.unwrap().unwrap();
    assert_eq!(card.step, "wont_do");
    assert!(card.worker_session_id.is_none());
    assert_eq!(card.block_reason.as_deref(), Some("scope changed"));
    // `wont_do` is terminal but NOT `done` — completed_at stamping is
    // intentionally for the success terminal only.
    assert!(card.completed_at.is_none());
}

#[tokio::test]
async fn wont_do_refuses_to_overturn_done() {
    let state = build_state().await;
    let (card_id, session_id) = seed_card_with_worker(&state, "in_progress", "task", "task").await;
    state
        .db
        .update_card(
            &card_id,
            UpdateCard {
                step: Some("done".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let registry = McpToolRegistry::new();
    let ctx = ctx_for_card(&state, &session_id, &card_id);

    let err = registry
        .handle_tool_call("wont_do_card", serde_json::json!({ "reason": "x" }), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("already done"), "got: {err}");
}

#[tokio::test]
async fn handle_worker_done_after_wont_do_is_idempotent() {
    let state = build_state().await;
    let (card_id, session_id) = seed_card_with_worker(&state, "in_progress", "task", "task").await;
    let registry = McpToolRegistry::new();
    let ctx = ctx_for_card(&state, &session_id, &card_id);

    registry
        .handle_tool_call(
            "wont_do_card",
            serde_json::json!({ "reason": "blocked" }),
            &ctx,
        )
        .await
        .unwrap();
    let before = state.db.get_card(&card_id).await.unwrap().unwrap();

    handle_worker_done(&state, &session_id).await;

    let after = state.db.get_card(&card_id).await.unwrap().unwrap();
    assert_eq!(after.step, before.step);
    assert_eq!(after.block_reason, before.block_reason);
}

// ── concurrency / races ──────────────────────────────────────────────

#[tokio::test]
async fn concurrent_finish_calls_are_idempotent() {
    // Two finish_card calls racing on the same session shouldn't
    // corrupt the card. The DB-level update_card_atomic serialises
    // them, and the terminal-guard makes the second call a no-op (or a
    // documented refusal). Either way, the card ends up at `done` with
    // a sensible handoff_context and a single `step-change` describing
    // the actual transition that happened.
    let state = build_state().await;
    let (card_id, session_id) = seed_card_with_worker(&state, "in_progress", "task", "task").await;
    let registry = Arc::new(McpToolRegistry::new());

    let ctx_a = ctx_for_card(&state, &session_id, &card_id);
    let ctx_b = ctx_for_card(&state, &session_id, &card_id);
    let reg_a = registry.clone();
    let reg_b = registry.clone();

    let a = tokio::spawn(async move {
        reg_a
            .handle_tool_call("finish_card", serde_json::json!({ "summary": "A" }), &ctx_a)
            .await
    });
    let b = tokio::spawn(async move {
        reg_b
            .handle_tool_call("finish_card", serde_json::json!({ "summary": "B" }), &ctx_b)
            .await
    });
    let (ra, rb) = tokio::join!(a, b);
    let ra = ra.unwrap();
    let rb = rb.unwrap();

    // At least one of the two must succeed. `finish_card` on an already-
    // `done` card is allowed (idempotent re-finish), so both can also
    // succeed.
    assert!(
        ra.is_ok() || rb.is_ok(),
        "at least one concurrent finish_card must succeed"
    );

    let card = state.db.get_card(&card_id).await.unwrap().unwrap();
    assert_eq!(card.step, "done");
    assert!(card.worker_session_id.is_none());

    // Exactly one transition from in_progress → done happened. The
    // second call would emit a step-change with from == "done", which
    // is fine (still no-op for derive_worker_intent), but the first one
    // describes the actual jump.
    let events = state
        .db
        .list_events_by_session(&session_id, None)
        .await
        .unwrap();
    let from_in_progress = events
        .iter()
        .filter(|e| e.kind == "step-change")
        .filter(|e| {
            serde_json::from_str::<serde_json::Value>(&e.data)
                .ok()
                .and_then(|v| v.get("from").and_then(|s| s.as_str()).map(str::to_string))
                .as_deref()
                == Some("in_progress")
        })
        .count();
    assert_eq!(
        from_in_progress, 1,
        "exactly one transition from the pre-state happened"
    );
}

#[tokio::test]
async fn complete_step_then_finish_yields_done_not_skip() {
    // Sequence the worker might run if it decides the rest of the
    // pipeline is unnecessary after handing off step 1: complete_step,
    // then finish_card. After the MCP handler advances to step 2
    // (review) and the user-facing card transitions to review, the
    // follow-up finish_card must land on `done` from `review` — not
    // attempt to re-advance from some intermediate state.
    let state = build_state().await;
    let (card_id, session_id) = seed_card_with_worker(
        &state,
        "in_progress",
        "deep-develop-software",
        "deep-develop-software",
    )
    .await;
    let registry = McpToolRegistry::new();
    let ctx = ctx_for_card(&state, &session_id, &card_id);

    registry
        .handle_tool_call(
            "complete_step",
            serde_json::json!({ "handoff_context": "phase 1 done" }),
            &ctx,
        )
        .await
        .unwrap();
    // After complete_step the orchestrator would normally re-assign the
    // card to a new session for the review step. We simulate that here
    // because the agent that calls finish_card next is the reviewer
    // session, not the original.
    let review_session = uuid::Uuid::new_v4().to_string();
    let ts = chrono::Utc::now().to_rfc3339();
    state
        .db
        .create_session(NewSession {
            id: review_session.clone(),
            name: "reviewer".into(),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker: true,
            project_id: Some("p1".into()),
            card_id: Some(card_id.clone()),
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();
    state
        .db
        .update_card(
            &card_id,
            UpdateCard {
                worker_session_id: Some(Some(review_session.clone())),
                last_worker_session_id: Some(Some(review_session.clone())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let ctx2 = ctx_for_card(&state, &review_session, &card_id);
    registry
        .handle_tool_call(
            "finish_card",
            serde_json::json!({ "summary": "reviewed" }),
            &ctx2,
        )
        .await
        .unwrap();

    let card = state.db.get_card(&card_id).await.unwrap().unwrap();
    assert_eq!(card.step, "done");
    assert_eq!(card.handoff_context.as_deref(), Some("reviewed"));
    assert_eq!(
        card.last_worker_session_id.as_deref(),
        Some(review_session.as_str()),
        "the most-recent worker is the reviewer"
    );
}
