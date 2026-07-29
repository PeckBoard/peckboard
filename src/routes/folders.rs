use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{delete, get, post},
};
use serde::Deserialize;
use std::sync::Arc;

use crate::auth::middleware::{require_admin, require_auth, require_session_access};
use crate::db::crud::MoveFolderOutcome;
use crate::db::models::NewFolder;
use crate::service::doc_review_sources as sources;
use crate::service::fs_jail;
use crate::service::repo_scan;
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
        .merge(admin_router())
        .merge(user_router(state.clone()))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// Routes that register or unregister a workspace root. A folder is the cwd
/// and the file-access scope of every agent spawned inside it, so creating one
/// hands out host file access and deleting one destroys another user's work.
/// Neither is partitioned per user, so both are admin-only — same reasoning as
/// `routes/settings.rs`.
///
/// Layers run outer-to-inner on the request, so `require_admin` is appended
/// here and `require_auth` in [`router`] afterwards, which puts `AuthUser`
/// into the extensions before this middleware reads it.
fn admin_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/folders", post(create_folder))
        .route("/api/folders/{id}", delete(delete_folder))
        .route(
            "/api/folders/{id}/delete-sessions",
            post(delete_with_sessions),
        )
        .route(
            "/api/folders/{id}/move-sessions",
            post(move_sessions_then_delete),
        )
        .route_layer(middleware::from_fn(require_admin))
}

/// Routes any authenticated user may call. Listing folders is how a non-admin
/// finds the workspaces they're allowed to work in, and the per-entity move
/// routes only shuffle work between folders that already exist — neither
/// widens the file-access scope the admin chose.
fn user_router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/folders", get(list_folders))
        .merge(session_folder_router(state))
        // Per-entity folder change: each route cancels any agent the
        // move affects BEFORE the DB write, so the move is durable
        // within the running process. Process restart kills child
        // agents independently, so there's no need for a separate
        // DB-marker / replay step.
        .route("/api/projects/{id}/folder", post(change_project_folder))
        .route(
            "/api/repeating-tasks/{id}/folder",
            post(change_repeating_task_folder),
        )
        // Jailed, read-only markdown access inside a folder: the source
        // picker for Document Review. Any authenticated user may already
        // start a session in a folder (and read files from there), so
        // reading its markdown widens nothing.
        .route("/api/folders/{id}/markdown-files", get(list_markdown_files))
        .route("/api/folders/{id}/markdown-file", get(get_markdown_file))
        // Git repos across every folder, each with its worktrees — the
        // review wizard's repo → worktree → file cascade. Read-only, same
        // audience as the markdown routes above.
        .route("/api/repos", get(list_repos))
}

/// Moving a session cancels its running agent and repoints the workspace
/// it runs in, so it is a write on that session and carries the same
/// owner-or-shared-board gate as the session routes
/// (`require_session_access`). Kept in its own router because the sibling
/// `{id}` routes above name projects, repeating tasks, and folders — not
/// sessions — and must not be looked up as one.
fn session_folder_router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/sessions/{id}/folder", post(change_session_folder))
        .route_layer(middleware::from_fn_with_state(
            state,
            require_session_access,
        ))
}

/// Directory trees that must never become an agent workspace, even for an
/// admin. Registering one points every agent spawned in that folder at the
/// host's system files. Both the raw and the canonicalized path are checked
/// against this list, because macOS canonicalizes `/var` to `/private/var`.
const SYSTEM_PATH_PREFIXES: &[&str] = &[
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/lib",
    "/lib32",
    "/lib64",
    "/proc",
    "/private/etc",
    "/private/var",
    "/run",
    "/sbin",
    "/sys",
    "/usr",
    "/var",
];

/// Refuse workspace roots that are obviously dangerous: the filesystem root,
/// the system tree, and PeckBoard's own data directory (an agent editing the
/// live DB out from under the server corrupts it).
///
/// This is a hard refusal rather than a warning: the create API has no way to
/// carry an "I understand" acknowledgement, and nothing legitimate lives in
/// these trees. Arbitrary project directories under the user's home — the
/// point of a local tool — are unaffected, including the home directory
/// itself, which is an ancestor of the data dir but not inside it.
fn reject_unsafe_path(path: &std::path::Path, data_dir: &std::path::Path) -> Result<(), String> {
    // Resolve symlinks and `..` so `/tmp/../etc` and a symlink to /etc are
    // caught too. An unresolvable path (not created yet) falls back to the
    // literal one, which still catches the plain `/etc` spelling.
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

    if resolved.parent().is_none() {
        return Err("refusing to register the filesystem root as a folder".into());
    }

    for prefix in SYSTEM_PATH_PREFIXES {
        if resolved.starts_with(prefix) || path.starts_with(prefix) {
            return Err(format!(
                "refusing to register a system path as a folder: {}",
                resolved.display()
            ));
        }
    }

    let data_dir = data_dir
        .canonicalize()
        .unwrap_or_else(|_| data_dir.to_path_buf());
    if resolved.starts_with(&data_dir) {
        return Err(format!(
            "refusing to register PeckBoard's own data directory as a folder: {}",
            data_dir.display()
        ));
    }

    Ok(())
}

/// POST /api/folders
async fn create_folder(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateFolderRequest>,
) -> impl IntoResponse {
    tracing::info!(name = %body.name, path = %body.path, create = body.create.unwrap_or(false), "Creating folder");
    let path = std::path::Path::new(&body.path);

    // Checked before the create branch below, so a refused path is never
    // created on disk as a side effect of the refusal.
    if let Err(msg) = reject_unsafe_path(path, &state.config.data_dir) {
        tracing::warn!(path = %body.path, "Refused unsafe folder path");
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": msg })),
        ));
    }

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
/// Resolve a folder's canonical on-disk root, or the response to send back.
async fn markdown_root(
    state: &AppState,
    folder_id: &str,
) -> Result<std::path::PathBuf, (StatusCode, Json<serde_json::Value>)> {
    sources::folder_root(&state.db, folder_id)
        .await
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": e })),
            )
        })
}

/// GET /api/folders/{id}/markdown-files — every `.md` file under the folder,
/// as paths relative to its root. Bounded by the shared jail's depth and file
/// caps; `truncated` says the listing was cut short. Symlinks are not
/// followed, so the list can never name a file outside the folder.
///
/// `?scope=<rel/dir>` walks only that subdirectory — the review wizard
/// passes a repo worktree's folder-relative path here, including trees the
/// normal walk never enters (`.peckboard/worktrees/<id8>` sits inside a
/// hidden dir). Paths come back prefixed with `<scope>/`, so they slot into
/// `source_ref`, the preview read and apply unchanged. `?worktree=<id8>` is
/// the legacy spelling of `scope=.peckboard/worktrees/<id8>`.
async fn list_markdown_files(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<MarkdownListQuery>,
) -> impl IntoResponse {
    let folder_root = markdown_root(&state, &id).await?;
    let (root, prefix) = match (q.scope.as_deref(), q.worktree.as_deref()) {
        (Some(_), Some(_)) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "pass either scope or worktree, not both" })),
            ));
        }
        (None, None) => (folder_root, String::new()),
        (None, Some(id8)) => {
            if !repo_scan::is_worktree_id8(id8) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": "invalid worktree id" })),
                ));
            }
            scoped_walk_root(
                &folder_root,
                &format!(".peckboard/worktrees/{id8}"),
                "worktree",
            )?
        }
        (Some(scope), None) => {
            // Trailing-slash tolerance only — a LEADING slash must reach
            // check_relative and be refused as absolute.
            let scope = scope.trim_end_matches('/');
            if scope.is_empty() {
                // An empty scope is the folder itself — same as no scope.
                (folder_root, String::new())
            } else {
                scoped_walk_root(&folder_root, scope, "scope")?
            }
        }
    };
    let (walked, truncated) = fs_jail::walk_files(&root, &|p| sources::is_markdown(p));
    let files: Vec<serde_json::Value> = walked
        .into_iter()
        .map(|f| {
            serde_json::json!({
                "path": format!("{prefix}{}", f.path.replace('\\', "/")),
                "size": f.size,
            })
        })
        .collect();
    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({
        "files": files,
        "truncated": truncated,
    })))
}

/// Resolve a folder-relative directory as a walk root: the path must be
/// relative and descending, and its canonicalization must stay inside the
/// folder — a symlinked dir must not walk elsewhere, same reasoning as the
/// jailed read. Returns the canonical root and the `"<rel>/"` path prefix.
fn scoped_walk_root(
    folder_root: &std::path::Path,
    rel: &str,
    what: &str,
) -> Result<(std::path::PathBuf, String), (StatusCode, Json<serde_json::Value>)> {
    if fs_jail::check_relative(std::path::Path::new(rel)).is_err() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid {what} path") })),
        ));
    }
    let canon = std::fs::canonicalize(folder_root.join(rel)).map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("{what} not found") })),
        )
    })?;
    if !canon.starts_with(folder_root) || !canon.is_dir() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("{what} escapes the folder") })),
        ));
    }
    Ok((canon, format!("{rel}/")))
}

#[derive(Deserialize)]
struct MarkdownListQuery {
    /// Legacy: scope the listing to one root-level card worktree by its dir
    /// name (8-hex id8).
    worktree: Option<String>,
    /// Folder-relative directory to walk instead of the whole folder — how
    /// the wizard scopes the listing to one repo worktree.
    scope: Option<String>,
}

#[derive(Deserialize)]
struct MarkdownFileQuery {
    path: String,
}

/// GET /api/folders/{id}/markdown-file?path=rel.md — read one markdown file
/// inside the folder, capped at 1 MiB. The path must be relative, `.md`, and
/// resolve (after canonicalization) inside the folder root.
async fn get_markdown_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<MarkdownFileQuery>,
) -> impl IntoResponse {
    let root = markdown_root(&state, &id).await?;
    if !sources::is_markdown(std::path::Path::new(&q.path)) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "only .md files can be read" })),
        ));
    }
    match sources::read_markdown(&root, &q.path) {
        Ok(markdown) => Ok(Json(serde_json::json!({
            "path": q.path,
            "markdown": markdown,
        }))),
        Err(e) => {
            let status = if e.contains("read limit") {
                StatusCode::PAYLOAD_TOO_LARGE
            } else if e.contains("file not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            Err((status, Json(serde_json::json!({ "error": e }))))
        }
    }
}

/// GET /api/repos — every git repo across every workspace folder, each with
/// its worktrees: the main checkout, git-registered linked trees, and fixed
/// `.peckboard/worktrees/<id8>` card trees whose git registration is gone.
/// The scan and its jail rules live in [`repo_scan`]. Card worktree dirs are
/// named by the card's first 8 UUID chars, so the dir name resolves to a
/// human label; a pruned card leaves it null and the branch still identifies
/// the tree.
async fn list_repos(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let folders = match state.db.list_folders().await {
        Ok(f) => f,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            ));
        }
    };
    let mut repos = Vec::new();
    for folder in folders {
        // The scan wants the same canonical root the markdown jail uses.
        let Ok(root) = std::fs::canonicalize(&folder.path) else {
            continue; // unreadable folder — nothing to serve from it
        };
        for repo in repo_scan::scan_repos(&root) {
            let mut worktrees = Vec::with_capacity(repo.worktrees.len());
            for wt in &repo.worktrees {
                // A card tree is named by its card's id8 — resolve a label.
                let id8 = std::path::Path::new(&wt.path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .filter(|n| !wt.main && repo_scan::is_worktree_id8(n));
                let card = match &id8 {
                    Some(id8) => state.db.find_card_by_id_prefix(id8).await.ok().flatten(),
                    None => None,
                };
                worktrees.push(serde_json::json!({
                    "path": wt.path,
                    "branch": wt.branch,
                    "main": wt.main,
                    "card_id": card.as_ref().map(|c| c.id.clone()),
                    "card_title": card.as_ref().map(|c| c.title.clone()),
                }));
            }
            repos.push(serde_json::json!({
                "folder_id": folder.id,
                "folder_name": folder.name,
                "path": repo.path,
                // The folder-root repo has no dir name worth showing — the
                // folder's own name is the repo's name there.
                "name": if repo.path.is_empty() {
                    folder.name.clone()
                } else {
                    repo.name.clone()
                },
                "worktrees": worktrees,
            }));
        }
    }
    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({ "repos": repos })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::{seed_authenticated_user, seed_session, test_state};
    use axum::body::Body;
    use axum::http::{Request, header};
    use tower::ServiceExt;

    fn app(state: Arc<AppState>) -> Router {
        Router::new().merge(router(state.clone())).with_state(state)
    }

    fn create_request(token: &str, name: &str, path: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/folders")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "name": name, "path": path }).to_string(),
            ))
            .unwrap()
    }

    async fn error_message(response: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        json["error"].as_str().unwrap_or_default().to_string()
    }

    #[tokio::test]
    async fn non_admin_cannot_create_or_delete_folders() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "user").await;
        let workspace = dir.path().join("work");
        std::fs::create_dir_all(&workspace).unwrap();

        let response = app(state.clone())
            .oneshot(create_request(&token, "probe", workspace.to_str().unwrap()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(state.db.list_folders().await.unwrap().is_empty());

        let delete = Request::builder()
            .method("DELETE")
            .uri("/api/folders/whatever")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = app(state.clone()).oneshot(delete).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        // Listing stays open — a non-admin still has to find their workspace.
        let list = Request::builder()
            .uri("/api/folders")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = app(state).oneshot(list).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn admin_can_create_and_delete_a_project_folder() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;
        // The data dir is off limits, so the workspace lives beside it.
        let workspace = tempfile::tempdir().unwrap();

        let response = app(state.clone())
            .oneshot(create_request(
                &token,
                "projects",
                workspace.path().to_str().unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let folders = state.db.list_folders().await.unwrap();
        assert_eq!(folders.len(), 1);

        let delete = Request::builder()
            .method("DELETE")
            .uri(format!("/api/folders/{}", folders[0].id))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = app(state.clone()).oneshot(delete).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(state.db.list_folders().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn admin_cannot_register_system_paths_or_the_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;

        for path in ["/etc", "/", "/usr/share"] {
            let response = app(state.clone())
                .oneshot(create_request(&token, "probe", path))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "path {path}");
            assert!(
                error_message(response).await.contains("refusing"),
                "path {path} should be refused by the unsafe-path guard"
            );
        }

        let data_dir = dir.path().to_str().unwrap().to_string();
        let response = app(state.clone())
            .oneshot(create_request(&token, "data", &data_dir))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(error_message(response).await.contains("data directory"));

        assert!(state.db.list_folders().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn unsafe_path_is_refused_before_the_directory_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;
        let inside_data_dir = dir.path().join("agent-workspace");

        let request = Request::builder()
            .method("POST")
            .uri("/api/folders")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "name": "probe",
                    "path": inside_data_dir.to_str().unwrap(),
                    "create": true,
                })
                .to_string(),
            ))
            .unwrap();

        let response = app(state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            !inside_data_dir.exists(),
            "a refused path must not be created on disk"
        );
    }

    /// Seed a folder `f1` rooted at `workspace` and return a user token.
    async fn markdown_fixture(state: &Arc<AppState>, workspace: &std::path::Path) -> String {
        let token = seed_authenticated_user(state, "user").await;
        state
            .db
            .create_folder(crate::db::models::NewFolder {
                id: "f1".into(),
                name: "Repo".into(),
                path: workspace.to_string_lossy().to_string(),
                created_at: chrono::Utc::now().to_rfc3339(),
            })
            .await
            .unwrap();
        token
    }

    fn get(uri: &str, token: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn markdown_listing_covers_the_tree_and_skips_ignored_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = markdown_fixture(&state, workspace.path()).await;
        let root = workspace.path();
        for sub in ["docs", "docs/deep", "node_modules", ".git"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        std::fs::write(root.join("README.md"), "# readme").unwrap();
        std::fs::write(root.join("docs/deep/plan.md"), "# plan").unwrap();
        std::fs::write(root.join("docs/notes.txt"), "plain").unwrap();
        std::fs::write(root.join("node_modules/dep.md"), "# vendored").unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref").unwrap();

        let response = app(state.clone())
            .oneshot(get("/api/folders/f1/markdown-files", &token))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let paths: Vec<String> = json["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap().to_string())
            .collect();
        assert!(paths.contains(&"README.md".to_string()), "{paths:?}");
        assert!(
            paths.contains(&"docs/deep/plan.md".to_string()),
            "{paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.ends_with(".txt")),
            "markdown only: {paths:?}"
        );
        assert!(
            !paths
                .iter()
                .any(|p| p.contains("node_modules") || p.contains(".git")),
            "ignored dirs stay hidden: {paths:?}"
        );

        // An unregistered folder is a 404, not a path probe.
        let response = app(state)
            .oneshot(get("/api/folders/nope/markdown-files", &token))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn markdown_read_is_jailed_capped_and_markdown_only() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = markdown_fixture(&state, workspace.path()).await;
        let root = workspace.path();
        std::fs::write(root.join("doc.md"), "# doc\n\nbody\n").unwrap();
        std::fs::write(root.join("notes.txt"), "plain").unwrap();
        std::fs::write(root.join("huge.md"), "x".repeat(1024 * 1024 + 1)).unwrap();
        std::fs::write(
            root.parent().unwrap().join("folder_secret.md"),
            "# TOP SECRET\n",
        )
        .unwrap();

        let response = app(state.clone())
            .oneshot(get("/api/folders/f1/markdown-file?path=doc.md", &token))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json["markdown"].as_str().unwrap().contains("body"));

        for (query, expected, why) in [
            (
                "path=../folder_secret.md",
                StatusCode::BAD_REQUEST,
                "traversal",
            ),
            ("path=/etc/hosts", StatusCode::BAD_REQUEST, "absolute path"),
            ("path=notes.txt", StatusCode::BAD_REQUEST, "non-markdown"),
            ("path=missing.md", StatusCode::NOT_FOUND, "missing file"),
            (
                "path=huge.md",
                StatusCode::PAYLOAD_TOO_LARGE,
                "over the 1 MiB cap",
            ),
        ] {
            let response = app(state.clone())
                .oneshot(get(
                    &format!("/api/folders/f1/markdown-file?{query}"),
                    &token,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{why} must be refused");
        }

        // A symlink whose textual path looks in-bounds is refused by the
        // canonicalized containment check.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                root.parent().unwrap().join("folder_secret.md"),
                root.join("link.md"),
            )
            .unwrap();
            let response = app(state.clone())
                .oneshot(get("/api/folders/f1/markdown-file?path=link.md", &token))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert!(error_message(response).await.contains("escapes"));
        }

        let _ = std::fs::remove_file(root.parent().unwrap().join("folder_secret.md"));
    }

    fn move_session_request(token: &str, session_id: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(format!("/api/sessions/{session_id}/folder"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "target_folder_id": "f2" }).to_string(),
            ))
            .unwrap()
    }

    /// `seed_session` puts the session in folder `f1`; this adds the folder
    /// the move targets.
    async fn seed_target_folder(state: &Arc<AppState>) {
        state
            .db
            .create_folder(NewFolder {
                id: "f2".into(),
                name: "f2".into(),
                path: "/tmp/f2".into(),
                created_at: chrono::Utc::now().to_rfc3339(),
            })
            .await
            .unwrap();
    }

    /// Moving a session cancels whatever agent is running in it and
    /// repoints the workspace it runs in, so it is a write on that session
    /// and carries the same gate as the other session routes: a non-owner
    /// gets the indistinguishable 404 and the row does not move.
    #[tokio::test]
    async fn non_owner_cannot_move_another_users_session() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "user").await;
        seed_session(&state, "s-theirs", Some("u2"), None).await;
        seed_target_folder(&state).await;

        let response = app(state.clone())
            .oneshot(move_session_request(&token, "s-theirs"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(error_message(response).await, "session not found");

        let session = state.db.get_session("s-theirs").await.unwrap().unwrap();
        assert_eq!(session.folder_id, "f1", "session must not have moved");
    }

    #[tokio::test]
    async fn owner_can_move_their_own_session() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "user").await;
        seed_session(&state, "s-mine", Some("u1"), None).await;
        seed_target_folder(&state).await;

        let response = app(state.clone())
            .oneshot(move_session_request(&token, "s-mine"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let session = state.db.get_session("s-mine").await.unwrap().unwrap();
        assert_eq!(session.folder_id, "f2");
    }

    #[tokio::test]
    async fn repos_list_and_scoped_walks_stay_jailed() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = markdown_fixture(&state, workspace.path()).await;
        let root = std::fs::canonicalize(workspace.path()).unwrap();

        // A repo in a subfolder with one registered card worktree — fake git
        // metadata, no git binary: `.git/HEAD` names the branch,
        // `.git/worktrees/<name>/gitdir` registers the linked tree.
        let repo = root.join("repos/app");
        std::fs::create_dir_all(repo.join(".git/worktrees/deadbeef")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let tree = repo.join(".peckboard/worktrees/deadbeef");
        std::fs::create_dir_all(tree.join("docs")).unwrap();
        std::fs::write(
            repo.join(".git/worktrees/deadbeef/gitdir"),
            format!("{}/.git\n", tree.display()),
        )
        .unwrap();
        std::fs::write(
            repo.join(".git/worktrees/deadbeef/HEAD"),
            "ref: refs/heads/card/deadbeef\n",
        )
        .unwrap();
        std::fs::write(tree.join("docs/wt.md"), "# wt").unwrap();
        std::fs::write(tree.join("skip.txt"), "x").unwrap();
        std::fs::write(root.join("README.md"), "# readme").unwrap();
        // A legacy root-level card tree, addressed by `?worktree=<id8>`.
        std::fs::create_dir_all(root.join(".peckboard/worktrees/0a1b2c3d")).unwrap();
        std::fs::write(root.join(".peckboard/worktrees/0a1b2c3d/n.md"), "# n").unwrap();

        // The repo listing finds the subfolder repo and both of its trees.
        let response = app(state.clone())
            .oneshot(get("/api/repos", &token))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let repos = json["repos"].as_array().unwrap();
        assert_eq!(repos.len(), 1, "{json}");
        assert_eq!(repos[0]["folder_id"], "f1");
        assert_eq!(repos[0]["path"], "repos/app");
        assert_eq!(repos[0]["name"], "app");
        let wts = repos[0]["worktrees"].as_array().unwrap();
        let wt_rows: Vec<(&str, &str)> = wts
            .iter()
            .map(|w| (w["path"].as_str().unwrap(), w["branch"].as_str().unwrap()))
            .collect();
        assert_eq!(
            wt_rows,
            vec![
                ("repos/app", "main"),
                ("repos/app/.peckboard/worktrees/deadbeef", "card/deadbeef"),
            ],
            "{json}"
        );
        assert_eq!(wts[0]["main"], true);
        assert_eq!(wts[1]["main"], false);

        // The scoped walk sees ONLY the worktree, with prefixed paths…
        let response = app(state.clone())
            .oneshot(get(
                "/api/folders/f1/markdown-files?scope=repos/app/.peckboard/worktrees/deadbeef",
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let paths: Vec<String> = json["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            paths,
            vec!["repos/app/.peckboard/worktrees/deadbeef/docs/wt.md".to_string()],
            "{paths:?}"
        );

        // …the prefixed path reads back through the jailed single-file
        // route (the same resolver `source_ref` uses)…
        let response = app(state.clone())
            .oneshot(get(
                "/api/folders/f1/markdown-file?path=repos/app/.peckboard/worktrees/deadbeef/docs/wt.md",
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // …the unscoped walk still hides the hidden trees…
        let response = app(state.clone())
            .oneshot(get("/api/folders/f1/markdown-files", &token))
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            !json["files"].to_string().contains(".peckboard"),
            "hidden without a scope"
        );

        // …the legacy worktree param still scopes to the root-level layout…
        let response = app(state.clone())
            .oneshot(get(
                "/api/folders/f1/markdown-files?worktree=0a1b2c3d",
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json["files"][0]["path"].as_str().unwrap(),
            ".peckboard/worktrees/0a1b2c3d/n.md"
        );

        // …and bad scopes fail closed.
        for (uri, status) in [
            (
                "/api/folders/f1/markdown-files?scope=../escape",
                StatusCode::BAD_REQUEST,
            ),
            (
                "/api/folders/f1/markdown-files?scope=/etc",
                StatusCode::BAD_REQUEST,
            ),
            (
                "/api/folders/f1/markdown-files?scope=repos/missing",
                StatusCode::NOT_FOUND,
            ),
            (
                "/api/folders/f1/markdown-files?scope=repos/app&worktree=0a1b2c3d",
                StatusCode::BAD_REQUEST,
            ),
            (
                "/api/folders/f1/markdown-files?worktree=DEADBEEF",
                StatusCode::BAD_REQUEST,
            ),
            (
                "/api/folders/f1/markdown-files?worktree=ffffffff",
                StatusCode::NOT_FOUND,
            ),
        ] {
            let response = app(state.clone()).oneshot(get(uri, &token)).await.unwrap();
            assert_eq!(response.status(), status, "{uri}");
        }

        // A symlinked scope that resolves outside the folder is refused.
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
            let response = app(state)
                .oneshot(get("/api/folders/f1/markdown-files?scope=link", &token))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
    }
}
