//! `/api/admin/*` — restart the server, and see what a restart interrupts.
//!
//! `GET /api/admin/activity` lists the in-flight work a restart would kill
//! (see [`crate::service::restart::collect_activity`]); the UI shows it in
//! a confirmation before restarting. `POST /api/admin/restart` re-execs the
//! running binary — immediately, or with `?when=idle` once that list is
//! empty. `GET` reports the pending idle restart and `DELETE` cancels it.
//! All admin-only: a restart disconnects every user on the host.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::get,
};
use serde::Deserialize;

use crate::auth::middleware::{require_admin, require_auth};
use crate::service::restart::{self, RestartKind};
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/admin/activity", get(activity))
        .route(
            "/api/admin/restart",
            get(pending_restart)
                .post(restart_server)
                .delete(cancel_restart),
        )
        // Layers run outer-to-inner: `require_auth` puts `AuthUser` into the
        // extensions before `require_admin` reads it.
        .route_layer(middleware::from_fn(require_admin))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// `?when=idle` defers the restart until nothing is running; anything else
/// (or no query) restarts now.
#[derive(Debug, Default, Deserialize)]
pub struct WhenQuery {
    pub when: Option<String>,
}

impl WhenQuery {
    pub fn is_idle(&self) -> bool {
        self.when.as_deref() == Some("idle")
    }
}

fn error(status: StatusCode, msg: impl std::fmt::Display) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
        .into_response()
}

async fn activity(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match restart::collect_activity(&state).await {
        Ok(a) => Json(a).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

async fn pending_restart() -> impl IntoResponse {
    Json(serde_json::json!({ "pending": restart::pending() }))
}

async fn cancel_restart(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let cancelled = restart::cancel_pending(&state.broadcaster);
    Json(serde_json::json!({ "ok": true, "cancelled": cancelled }))
}

async fn restart_server(
    State(state): State<Arc<AppState>>,
    Query(q): Query<WhenQuery>,
) -> impl IntoResponse {
    let exe = match restart::current_exe_for_restart() {
        Ok(p) => p,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    if q.is_idle() {
        let pending =
            restart::schedule_idle_restart(state, exe, RestartKind::Restart, None, reexec);
        return Json(serde_json::json!({ "ok": true, "pending": pending })).into_response();
    }
    // A restart now supersedes one waiting for idle.
    restart::cancel_pending(&state.broadcaster);
    restart::restart_soon(exe, std::time::Duration::from_millis(600));
    Json(serde_json::json!({ "ok": true, "restarting": true })).into_response()
}

/// The production restart action handed to `schedule_idle_restart`.
pub fn reexec(exe: &std::path::Path) {
    if let Err(e) = crate::service::update::restart(exe) {
        tracing::error!("idle restart re-exec failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::{seed_authenticated_user, test_state};
    use crate::db::models::{NewFolder, NewSession};
    use axum::body::Body;
    use axum::http::{Request, header};
    use tower::ServiceExt;

    /// Reports a turn in flight for exactly the sessions it was given.
    struct BusyProvider(Vec<String>);

    #[async_trait::async_trait]
    impl crate::provider::agent::AgentProvider for BusyProvider {
        fn id(&self) -> &str {
            "busy"
        }
        async fn send_message(
            &self,
            _ctx: crate::provider::agent::SendMessageContext,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn cancel(&self, _session_id: &str) {}
        async fn interrupt(&self, _session_id: &str) {}
        async fn write_stdin(&self, _session_id: &str, _text: &str) -> bool {
            false
        }
        async fn is_running(&self, session_id: &str) -> bool {
            self.0.iter().any(|s| s == session_id)
        }
        async fn cleanup(&self) {}
        async fn shutdown(&self) {}
    }

    async fn get(state: &Arc<AppState>, token: &str, uri: &str) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let app = Router::new()
            .merge(router(state.clone()))
            .with_state(state.clone());
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn seed_session(state: &AppState, id: &str, name: &str) {
        state
            .db
            .create_session(NewSession {
                id: id.into(),
                name: name.into(),
                folder_id: "f1".into(),
                created_at: chrono::Utc::now().to_rfc3339(),
                last_activity: chrono::Utc::now().to_rfc3339(),
                ..Default::default()
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn activity_lists_mid_turn_sessions_and_background_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;
        state
            .db
            .create_folder(NewFolder {
                id: "f1".into(),
                name: "repo".into(),
                path: dir.path().to_string_lossy().into(),
                created_at: chrono::Utc::now().to_rfc3339(),
            })
            .await
            .unwrap();
        seed_session(&state, "s-busy", "Busy chat").await;
        seed_session(&state, "s-idle", "Idle chat").await;
        state
            .db
            .append_event("s-busy", "agent-start", serde_json::json!({}))
            .await
            .unwrap();
        state
            .provider_registry
            .register(
                Arc::new(BusyProvider(vec!["s-busy".into()])),
                crate::provider::registry::ProviderInfo {
                    id: "busy".into(),
                    display_name: "busy".into(),
                    models: Vec::new(),
                    effort_levels: Vec::new(),
                    capabilities: Default::default(),
                },
            )
            .await;
        let task_id = state.background.insert_running_for_test("s-idle");

        let (status, body) = get(&state, &token, "/api/admin/activity").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["total"], 2);
        assert_eq!(body["counts"]["sessions"], 1);
        assert_eq!(body["counts"]["background_tasks"], 1);
        let s = &body["sessions"][0];
        assert_eq!(s["session_id"], "s-busy");
        assert_eq!(s["name"], "Busy chat");
        assert_eq!(s["folder_name"], "repo");
        assert!(s["running_secs"].is_u64());
        let t = &body["background_tasks"][0];
        assert_eq!(t["task_id"], task_id.as_str());
        assert_eq!(t["session_name"], "Idle chat");
    }

    #[tokio::test]
    async fn non_admin_cannot_reach_the_restart_routes() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "user").await;
        for uri in ["/api/admin/activity", "/api/admin/restart"] {
            let (status, _) = get(&state, &token, uri).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        }
    }

    /// The scheduled restart only fires once the activity list drains, and
    /// fires through the injected action — never by exiting the test.
    #[tokio::test]
    async fn idle_restart_waits_for_the_activity_list_to_drain() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let task_id = state.background.insert_running_for_test("s1");
        let (fired_tx, mut fired_rx) = tokio::sync::oneshot::channel::<()>();
        let mut fired_tx = Some(fired_tx);
        let pending = restart::schedule_idle_restart(
            state.clone(),
            "/nonexistent/peckboard".into(),
            RestartKind::Restart,
            None,
            move |_| {
                if let Some(tx) = fired_tx.take() {
                    let _ = tx.send(());
                }
            },
        );
        assert_eq!(pending.kind, RestartKind::Restart);
        assert!(restart::pending().is_some());

        // Busy: nothing fires across more than one poll interval.
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        assert!(fired_rx.try_recv().is_err(), "restarted while a task ran");
        assert_eq!(restart::pending().map(|p| p.remaining), Some(1));

        state.background.finish_for_test(&task_id);
        tokio::time::timeout(std::time::Duration::from_secs(15), fired_rx)
            .await
            .expect("restart never fired after going idle")
            .unwrap();
        assert!(restart::pending().is_none());
    }
}
