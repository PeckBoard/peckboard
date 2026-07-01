use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{delete, post},
};
use serde::Deserialize;
use std::sync::Arc;

use crate::auth::middleware::require_auth;
use crate::db::crud::MoveFolderOutcome;
use crate::db::models::NewFolder;
use crate::state::AppState;

#[derive(Deserialize)]
struct CreateFolderRequest {
    name: String,
    path: String,
    /// If true, create the directory on disk if it doesn't exist.
    create: Option<bool>,
}

#[derive(Deserialize)]
struct MoveSessionsRequest {
    target_folder_id: String,
}

/// Body shared by the per-entity move-folder routes.
#[derive(Deserialize)]
struct ChangeFolderRequest {
    target_folder_id: String,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/folders", post(create_folder).get(list_folders))
        .route("/api/folders/{id}", delete(delete_folder))
        .route(
            "/api/folders/{id}/delete-sessions",
            post(delete_with_sessions),
        )
        .route(
            "/api/folders/{id}/move-sessions",
            post(move_sessions_then_delete),
        )
        // Per-entity folder change: each route cancels any agent the
        // move affects BEFORE the DB write, so the move is durable
        // within the running process. Process restart kills child
        // agents independently, so there's no need for a separate
        // DB-marker / replay step.
        .route("/api/sessions/{id}/folder", post(change_session_folder))
        .route("/api/projects/{id}/folder", post(change_project_folder))
        .route(
            "/api/repeating-tasks/{id}/folder",
            post(change_repeating_task_folder),
        )
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// POST /api/folders
async fn create_folder(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateFolderRequest>,
) -> impl IntoResponse {
    tracing::info!(name = %body.name, path = %body.path, create = body.create.unwrap_or(false), "Creating folder");
    let path = std::path::Path::new(&body.path);
    if !path.exists() {
        if body.create.unwrap_or(false) {
            // Create the directory
            if let Err(e) = std::fs::create_dir_all(path) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(
                        serde_json::json!({ "error": format!("failed to create directory: {}", e) }),
                    ),
                ));
            }
            tracing::info!(path = %body.path, "Created directory on disk");
        } else {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(
                    serde_json::json!({ "error": format!("path does not exist: {}. Set create: true to create it.", body.path) }),
                ),
            ));
        }
    }

    let now = chrono::Utc::now().to_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();

    let folder = state
        .db
        .create_folder(NewFolder {
            id,
            name: body.name,
            path: body.path,
            created_at: now,
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    Ok((StatusCode::CREATED, Json(serde_json::json!(folder))))
}

/// GET /api/folders
async fn list_folders(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    tracing::info!("Listing folders");
    let mut folders = state.db.list_folders().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    // Hide the internal keep-alive folder — it's an implementation detail of
    // the login keep-alive, not a place the user picks work from.
    folders.retain(|f| f.id != crate::keepalive::KEEPALIVE_FOLDER_ID);

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(folders)))
}

/// DELETE /api/folders/:id
async fn delete_folder(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(folder_id = %id, "Deleting folder");

    // Atomic check-and-delete: prevents a concurrent session creation
    // from slipping in between an empty-check and the delete.
    let outcome = state.db.delete_folder_if_empty(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    match outcome {
        crate::db::crud::FolderEmptyDelete::Deleted => Ok(StatusCode::NO_CONTENT),
        crate::db::crud::FolderEmptyDelete::HasSessions(n) => Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "folder has active sessions",
                "session_count": n,
            })),
        )),
        crate::db::crud::FolderEmptyDelete::NotFound => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "folder not found" })),
        )),
    }
}

/// POST /api/folders/:id/delete-sessions — delete all sessions in folder, then delete folder
async fn delete_with_sessions(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(folder_id = %id, "Cascade-deleting folder and sessions");

    // Atomic cascade: sessions, their events, queued messages, and the
    // folder all drop in a single transactional closure. Replaces the
    // older "list → loop with let _ = …" pattern that silently dropped
    // failures and could leave orphaned rows behind.
    let report = state.db.delete_folder_cascade(&id).await.map_err(|e| {
        let msg = e.to_string();
        let status = if msg.contains("not found") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        (status, Json(serde_json::json!({ "error": msg })))
    })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({
        "deleted_sessions": report.sessions_deleted,
        "deleted_events": report.events_deleted,
    })))
}

/// POST /api/folders/:id/move-sessions — move sessions to target folder, then delete folder
async fn move_sessions_then_delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<MoveSessionsRequest>,
) -> impl IntoResponse {
    // Verify target folder exists
    let target = state
        .db
        .get_folder(&body.target_folder_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    if target.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "target folder not found" })),
        ));
    }

    // Move all sessions to target folder
    let moved = state
        .db
        .move_sessions_to_folder(&id, &body.target_folder_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    // Delete the now-empty folder
    state.db.delete_folder(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({
        "moved_sessions": moved
    })))
}

// ── Per-entity folder change ─────────────────────────────────────────
//
// Policy notes (also captured in the PM expert escalation):
// * Only sessions belonging to the entity being moved are cancelled.
//   We do NOT touch unrelated sessions in the destination folder.
// * Durability: we cancel running agents BEFORE the DB write and wait
//   for the cancel to land (`cancel_and_wait`), then update the rows
//   inside a single DB transaction. A mid-flight process crash leaves
//   the entity in its OLD folder (the DB write hadn't happened), and
//   process restart kills any still-attached agents anyway, so there
//   is no replay step to design around.
// * On success we revoke MCP tokens for every session whose folder
//   moved. The token already encodes the session id; downstream MCP
//   calls then re-derive the (now updated) folder_id at the route
//   layer, so a stale token can't be used to act in the old folder.

/// Cancel every session listed, wait for the cancel to fully wind
/// down, drop any queued message, and revoke its MCP tokens. Used by
/// the three change-folder routes. We block on the cancel so the DB
/// row that follows can be written atomically with no in-flight agent
/// holding a stale folder.
async fn cancel_sessions_and_revoke_tokens(state: &AppState, session_ids: &[String]) {
    for sid in session_ids {
        if state.session_manager.is_running(sid).await {
            state.session_manager.cancel_and_wait(sid).await;
        }
        // Drop any queued message so a deferred resume can't pop into
        // the agent process after the DB row was rewritten to a new
        // folder. The move helpers also clear the queued_messages row;
        // doing both is safe and inexpensive.
        let _ = state.db.delete_queued_message(sid).await;
        state.mcp_tokens.revoke_by_session(sid).await;
    }
}

/// POST /api/sessions/:id/folder — move a single plain (non-worker,
/// non-expert) session into a different folder. Worker / expert
/// sessions are owned by their project and must be moved via the
/// project endpoint; trying to move one here returns 409.
async fn change_session_folder(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ChangeFolderRequest>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, target = %body.target_folder_id, "Changing session folder");

    // Cancel the agent for this session first if anything is running.
    // We cancel BEFORE the DB write so the agent never sees the new
    // folder in mid-turn — if the cancel hangs we'd rather fail loud
    // than silently change folder under a live agent.
    cancel_sessions_and_revoke_tokens(&state, std::slice::from_ref(&id)).await;

    let outcome = state
        .db
        .move_session_to_folder(&id, &body.target_folder_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    match outcome {
        MoveFolderOutcome::Moved(session) => {
            state
                .broadcaster
                .broadcast(crate::ws::broadcaster::WsEvent {
                    event_type: "session-folder-changed".into(),
                    session_id: id.clone(),
                    data: serde_json::json!({ "session": &session }),
                });
            Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(session)))
        }
        MoveFolderOutcome::NotFound => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        )),
        MoveFolderOutcome::TargetMissing => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "target folder not found" })),
        )),
        MoveFolderOutcome::RefusedOwnedSession => Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "worker / expert sessions are owned by their project; \
                         move the project instead",
            })),
        )),
    }
}

/// POST /api/projects/:id/folder — move a project (and every session it
/// owns: workers + experts) into a different folder.
async fn change_project_folder(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ChangeFolderRequest>,
) -> impl IntoResponse {
    tracing::info!(project_id = %id, target = %body.target_folder_id, "Changing project folder");

    // Gather every owned session up-front so we know exactly who to
    // cancel. We do this BEFORE the cancel so a session that drops
    // mid-cancel doesn't get missed.
    let owned: Vec<String> = match state.db.list_sessions_by_project(&id).await {
        Ok(rows) => rows.into_iter().map(|s| s.id).collect(),
        Err(_) => Vec::new(),
    };
    cancel_sessions_and_revoke_tokens(&state, &owned).await;

    let outcome = state
        .db
        .move_project_to_folder(&id, &body.target_folder_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    match outcome {
        MoveFolderOutcome::Moved(report) => {
            // Also cancel any sessions the cascade scooped up that
            // weren't in the initial gather (a session created between
            // the gather and the cancel — race window is tiny but real).
            let extra: Vec<String> = report
                .owned_session_ids
                .iter()
                .filter(|sid| !owned.contains(sid))
                .cloned()
                .collect();
            if !extra.is_empty() {
                cancel_sessions_and_revoke_tokens(&state, &extra).await;
            }
            state
                .broadcaster
                .broadcast(crate::ws::broadcaster::WsEvent {
                    event_type: "project-folder-changed".into(),
                    session_id: id.clone(),
                    data: serde_json::json!({
                        "project": &report.project,
                        "previous_folder_id": report.previous_folder_id,
                        "sessions_moved": report.sessions_moved,
                    }),
                });
            Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({
                "project": report.project,
                "previous_folder_id": report.previous_folder_id,
                "sessions_moved": report.sessions_moved,
            })))
        }
        MoveFolderOutcome::NotFound => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "project not found" })),
        )),
        MoveFolderOutcome::TargetMissing => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "target folder not found" })),
        )),
        MoveFolderOutcome::RefusedOwnedSession => unreachable!(),
    }
}

/// POST /api/repeating-tasks/:id/folder — move a repeating task to a
/// different folder, dragging any sessions it spawned along with it.
async fn change_repeating_task_folder(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ChangeFolderRequest>,
) -> impl IntoResponse {
    tracing::info!(task_id = %id, target = %body.target_folder_id, "Changing repeating task folder");

    let owned: Vec<String> = match state.db.list_sessions_by_repeating_task(&id).await {
        Ok(rows) => rows.into_iter().map(|s| s.id).collect(),
        Err(_) => Vec::new(),
    };
    cancel_sessions_and_revoke_tokens(&state, &owned).await;

    let outcome = state
        .db
        .move_repeating_task_to_folder(&id, &body.target_folder_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    match outcome {
        MoveFolderOutcome::Moved(report) => {
            let extra: Vec<String> = report
                .owned_session_ids
                .iter()
                .filter(|sid| !owned.contains(sid))
                .cloned()
                .collect();
            if !extra.is_empty() {
                cancel_sessions_and_revoke_tokens(&state, &extra).await;
            }
            state
                .broadcaster
                .broadcast(crate::ws::broadcaster::WsEvent {
                    event_type: "repeating-task-folder-changed".into(),
                    session_id: id.clone(),
                    data: serde_json::json!({
                        "task": &report.task,
                        "previous_folder_id": report.previous_folder_id,
                        "sessions_moved": report.sessions_moved,
                    }),
                });
            Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({
                "task": report.task,
                "previous_folder_id": report.previous_folder_id,
                "sessions_moved": report.sessions_moved,
            })))
        }
        MoveFolderOutcome::NotFound => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "repeating task not found" })),
        )),
        MoveFolderOutcome::TargetMissing => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "target folder not found" })),
        )),
        MoveFolderOutcome::RefusedOwnedSession => unreachable!(),
    }
}
