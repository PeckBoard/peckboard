//! Read endpoints behind the operational dashboard widgets:
//!
//! - `GET /api/dashboard/background` — background tasks of the caller's
//!   sessions (admins: every session's).
//! - `GET /api/dashboard/workers?project_id=` — cards with a worker session.
//! - `GET /api/dashboard/worktrees?project_id=` — unmerged worktrees + the
//!   project folders' recent commits.
//! - `GET /api/dashboard/dependencies?project_id=&card_id=` — a project's
//!   card dependency graph and its top blockers.
//!
//! Visibility follows the existing rules: projects and cards are shared by
//! every logged-in user, sessions go through `may_access_session`.

use axum::{
    Extension, Json, Router,
    extract::{Query, State},
    http::StatusCode,
    middleware,
    routing::get,
};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::auth::access::may_access_session;
use crate::auth::middleware::{AuthUser, require_auth};
use crate::background::TaskStatus;
use crate::db::models::{Project, Session};
use crate::service::card_deps::{ProjectDeps, collect_transitive_deps, top_blockers};
use crate::state::AppState;

type ApiError = (StatusCode, Json<serde_json::Value>);

/// `background`: at most this many tasks.
const MAX_TASKS: usize = 30;
/// `worktrees`: commits read per repo, repos read when unscoped, and the
/// overall cap.
const COMMITS_PER_REPO: usize = 8;
const UNSCOPED_REPOS: usize = 5;
const MAX_COMMITS: usize = 30;
const GIT_TIMEOUT: Duration = Duration::from_secs(3);
/// `dependencies`: how many top blockers to report.
const TOP_BLOCKERS: usize = 5;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/dashboard/background", get(background))
        .route("/api/dashboard/workers", get(workers))
        .route("/api/dashboard/worktrees", get(worktrees))
        .route("/api/dashboard/dependencies", get(dependencies))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

#[derive(Deserialize)]
struct ScopeQuery {
    project_id: Option<String>,
}

#[derive(Deserialize)]
struct DepsQuery {
    project_id: Option<String>,
    card_id: Option<String>,
}

fn error(status: StatusCode, msg: &str) -> ApiError {
    (status, Json(serde_json::json!({ "error": msg })))
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}

fn may_see(user: &AuthUser, s: &Session) -> bool {
    may_access_session(
        user.is_admin(),
        &user.user_id,
        s.user_id.as_deref(),
        s.project_id.as_deref(),
    )
}

/// The one scoped project (404 when unknown), or every project.
async fn scoped_projects(
    state: &AppState,
    project_id: Option<&str>,
) -> Result<Vec<Project>, ApiError> {
    match project_id.filter(|p| !p.is_empty()) {
        Some(id) => {
            let p = state.db.get_project(id).await.map_err(internal)?;
            Ok(vec![p.ok_or_else(|| {
                error(StatusCode::NOT_FOUND, "project not found")
            })?])
        }
        None => state.db.list_projects().await.map_err(internal),
    }
}

/// GET /api/dashboard/background → `{"tasks": [..]}`: running tasks first
/// (newest first), then those finished within the registry's 24h retention
/// (most recently finished first), max [`MAX_TASKS`].
async fn background(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
) -> Json<serde_json::Value> {
    let mut tasks = state.background.list_all();
    tasks.sort_by(|a, b| {
        let running = |t: &crate::background::TaskInfo| t.status == TaskStatus::Running;
        running(b).cmp(&running(a)).then_with(|| {
            let at = |t: &crate::background::TaskInfo| {
                t.finished_at
                    .clone()
                    .unwrap_or_else(|| t.started_at.clone())
            };
            at(b).cmp(&at(a))
        })
    });

    // Session lookups are per distinct session; an unknown or inaccessible
    // session hides its tasks (same 404 rule as the per-task routes).
    let mut names: HashMap<String, Option<String>> = HashMap::new();
    let mut out = Vec::new();
    for t in tasks {
        if out.len() >= MAX_TASKS {
            break;
        }
        if !names.contains_key(&t.session_id) {
            let name = match state.db.get_session(&t.session_id).await {
                Ok(Some(s)) if may_see(&user, &s) => Some(s.name),
                _ => None,
            };
            names.insert(t.session_id.clone(), name);
        }
        let Some(name) = names.get(&t.session_id).cloned().flatten() else {
            continue;
        };
        out.push(serde_json::json!({
            "id": t.id,
            "session_id": t.session_id,
            "session_name": name,
            "label": t.label,
            "program": t.program,
            "status": t.status,
            "exit_code": t.exit_code,
            "started_at": t.started_at,
            "finished_at": t.finished_at,
            "stopping": t.stopping,
        }));
    }
    Json(serde_json::json!({ "tasks": out }))
}

/// RFC 3339 of an event's ms-epoch `ts`.
fn ms_to_rfc3339(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.to_rfc3339())
}

/// GET /api/dashboard/workers → `{"workers": [..]}`, one per card with a
/// worker session. `context_tokens` is the session's latest stored context
/// occupancy (`usage_events.context_tokens`, the same value that seeds the
/// board's per-card badge), `null` when none was recorded.
async fn workers(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Query(q): Query<ScopeQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let projects = scoped_projects(&state, q.project_id.as_deref()).await?;
    let mut out = Vec::new();
    for p in &projects {
        let cards = state
            .db
            .list_cards_by_project(&p.id)
            .await
            .map_err(internal)?;
        for c in cards {
            let Some(sid) = c.worker_session_id.as_deref() else {
                continue;
            };
            let session = state.db.get_session(sid).await.ok().flatten();
            if session.as_ref().is_some_and(|s| !may_see(&user, s)) {
                continue;
            }
            let run = state.db.latest_card_run(&c.id, sid).await.ok().flatten();
            let last_event = state
                .db
                .events_tail(sid, 1)
                .await
                .ok()
                .and_then(|evs| evs.last().and_then(|e| ms_to_rfc3339(e.ts)));
            let context_tokens = state
                .db
                .latest_context_tokens(sid)
                .await
                .ok()
                .flatten()
                .filter(|n| *n > 0);
            out.push(serde_json::json!({
                "session_id": sid,
                "session_name": session.as_ref().map(|s| s.name.clone()),
                "project_id": p.id,
                "project_name": p.name,
                "card_id": c.id,
                "card_title": c.title,
                "step": c.step,
                "model": run
                    .as_ref()
                    .and_then(|r| r.model.clone())
                    .or_else(|| session.as_ref().and_then(|s| s.model.clone())),
                "started_at": run.as_ref().map(|r| r.started_at.clone()),
                "running": state.session_manager.is_running(sid).await,
                "last_activity_at": last_event
                    .or_else(|| session.as_ref().map(|s| s.last_activity.clone())),
                "context_tokens": context_tokens,
            }));
        }
    }
    Ok(Json(serde_json::json!({ "workers": out })))
}

/// One parsed `git log` line.
struct Commit {
    sha: String,
    subject: String,
    author: String,
    date: chrono::DateTime<chrono::Utc>,
}

/// The newest [`COMMITS_PER_REPO`] commits of the repo at `folder`. `None`
/// for a non-git folder, a git failure, or a [`GIT_TIMEOUT`] overrun.
async fn recent_commits(folder: &str) -> Option<Vec<Commit>> {
    if !std::path::Path::new(folder).join(".git").exists() {
        return None;
    }
    let n = COMMITS_PER_REPO.to_string();
    let mut cmd = crate::sandbox::git_command_tokio(folder);
    cmd.args([
        "-C",
        folder,
        "log",
        "-n",
        &n,
        "--no-color",
        "--format=%H%x1f%s%x1f%an%x1f%aI",
    ])
    .stdin(std::process::Stdio::null())
    .kill_on_drop(true);
    let out = tokio::time::timeout(GIT_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let mut parts = line.split('\x1f');
                let sha = parts.next()?.to_string();
                let subject = parts.next()?.to_string();
                let author = parts.next()?.to_string();
                let date = chrono::DateTime::parse_from_rfc3339(parts.next()?)
                    .ok()?
                    .with_timezone(&chrono::Utc);
                Some(Commit {
                    sha,
                    subject,
                    author,
                    date,
                })
            })
            .collect(),
    )
}

/// GET /api/dashboard/worktrees → `{"unmerged": [..], "commits": [..]}`.
/// Commits come from the scoped project's folder, or (unscoped) the
/// [`UNSCOPED_REPOS`] most recently accessed projects'; a folder shared by
/// several projects is read once, under the first. Newest first, max
/// [`MAX_COMMITS`].
async fn worktrees(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ScopeQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut projects = scoped_projects(&state, q.project_id.as_deref()).await?;

    let mut unmerged = Vec::new();
    for p in &projects {
        let cards = state
            .db
            .list_cards_by_project(&p.id)
            .await
            .map_err(internal)?;
        for c in cards {
            let Some(reason) = c.worktree_unmerged_reason else {
                continue;
            };
            unmerged.push(serde_json::json!({
                "card_id": c.id,
                "title": c.title,
                "project_id": p.id,
                "project_name": p.name,
                "reason": reason,
                "detail": c.worktree_unmerged_detail,
                "updated_at": c.updated_at,
            }));
        }
    }
    unmerged.sort_by(|a, b| b["updated_at"].as_str().cmp(&a["updated_at"].as_str()));

    projects.sort_by(|a, b| b.last_accessed_at.cmp(&a.last_accessed_at));
    projects.truncate(UNSCOPED_REPOS);
    let mut seen_folders = HashSet::new();
    let mut commits: Vec<(Commit, &Project)> = Vec::new();
    for p in &projects {
        let Ok(Some(folder)) = state.db.get_folder(&p.folder_id).await else {
            continue;
        };
        if !seen_folders.insert(folder.path.clone()) {
            continue;
        }
        if let Some(list) = recent_commits(&folder.path).await {
            commits.extend(list.into_iter().map(|c| (c, p)));
        }
    }
    commits.sort_by_key(|(c, _)| std::cmp::Reverse(c.date));
    commits.truncate(MAX_COMMITS);
    let commits: Vec<_> = commits
        .into_iter()
        .map(|(c, p)| {
            serde_json::json!({
                "project_id": p.id,
                "project_name": p.name,
                "sha": c.sha,
                "subject": c.subject,
                "author": c.author,
                "date": c.date.to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(
        serde_json::json!({ "unmerged": unmerged, "commits": commits }),
    ))
}

/// GET /api/dashboard/dependencies?project_id=&card_id= → `{"nodes",
/// "edges", "top_blockers"}`. With `card_id`, the graph is that card plus
/// its transitive prerequisites; without, every card of the project that
/// has an edge. `blocked` is the card's manual block flag — waiting on an
/// unmet prerequisite shows as an edge to a not-`done` node.
async fn dependencies(
    State(state): State<Arc<AppState>>,
    Query(q): Query<DepsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let project_id = q
        .project_id
        .as_deref()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| error(StatusCode::BAD_REQUEST, "project_id is required"))?;
    if state
        .db
        .get_project(project_id)
        .await
        .map_err(internal)?
        .is_none()
    {
        return Err(error(StatusCode::NOT_FOUND, "project not found"));
    }

    let graph = ProjectDeps::load(&state.db, project_id)
        .await
        .map_err(internal)?;
    let mut deps_by_card = graph.deps_by_card();
    let mut info_by_id = graph.info_by_id();

    // Narrow the graph to the node set: the root's prerequisite closure, or
    // every card touching an edge.
    let keep: HashSet<String> = match q.card_id.as_deref().filter(|c| !c.is_empty()) {
        Some(root) => {
            if !info_by_id.contains_key(root) {
                return Err(error(StatusCode::NOT_FOUND, "card not found"));
            }
            let mut set = HashSet::new();
            collect_transitive_deps(root, &deps_by_card, &mut set);
            set.insert(root.to_string());
            set
        }
        None => graph
            .edges
            .iter()
            .flat_map(|(a, b)| [a.clone(), b.clone()])
            .collect(),
    };
    info_by_id.retain(|id, _| keep.contains(*id));
    deps_by_card.retain(|id, _| keep.contains(*id));
    for deps in deps_by_card.values_mut() {
        deps.retain(|d| keep.contains(*d));
    }

    let mut nodes: Vec<&crate::db::models::Card> = info_by_id.values().copied().collect();
    nodes.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    let nodes: Vec<_> = nodes
        .into_iter()
        .map(|c| {
            serde_json::json!({
                "card_id": c.id,
                "title": c.title,
                "step": c.step,
                "blocked": c.blocked,
                "done": c.step == "done",
            })
        })
        .collect();
    let edges: Vec<_> = graph
        .edges
        .iter()
        .filter(|(a, b)| info_by_id.contains_key(a.as_str()) && info_by_id.contains_key(b.as_str()))
        .map(|(a, b)| serde_json::json!({ "card_id": a, "depends_on": b }))
        .collect();
    let top: Vec<_> = top_blockers(&deps_by_card, &info_by_id, TOP_BLOCKERS)
        .into_iter()
        .map(|(id, blocks)| {
            serde_json::json!({
                "card_id": id,
                "title": info_by_id.get(id.as_str()).map(|c| c.title.clone()),
                "blocks": blocks,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "nodes": nodes,
        "edges": edges,
        "top_blockers": top,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::{seed_authenticated_user, seed_session, test_state};
    use crate::db::models::{NewCard, UpdateCard};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn get_json(
        state: &Arc<AppState>,
        token: &str,
        uri: &str,
    ) -> (StatusCode, serde_json::Value) {
        let res = router(state.clone())
            .with_state(state.clone())
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap_or_default())
    }

    async fn card(state: &AppState, id: &str, step: &str, update: UpdateCard) {
        let now = chrono::Utc::now().to_rfc3339();
        state
            .db
            .create_card(NewCard {
                id: id.into(),
                project_id: "p1".into(),
                title: format!("title {id}"),
                description: String::new(),
                step: step.into(),
                priority: 0,
                workflow: "default".into(),
                model: None,
                effort: None,
                blocked: false,
                block_reason: None,
                created_at: now.clone(),
                updated_at: now.clone(),
                system_prompt_name: None,
            })
            .await
            .unwrap();
        let update = UpdateCard {
            updated_at: Some(now),
            ..update
        };
        state.db.update_card(id, update).await.unwrap();
    }

    #[tokio::test]
    async fn background_lists_only_accessible_sessions_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "member").await;
        seed_session(&state, "s-mine", Some("u1"), None).await;
        seed_session(&state, "s-other", Some("u2"), None).await;
        let mine = state.background.insert_running_for_test("s-mine");
        let other = state.background.insert_running_for_test("s-other");

        let (status, body) = get_json(&state, &token, "/api/dashboard/background").await;
        assert_eq!(status, StatusCode::OK);
        let tasks = body["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1, "{body}");
        assert_eq!(tasks[0]["id"], mine);
        assert_eq!(tasks[0]["session_name"], "s-mine");
        assert_eq!(tasks[0]["status"], "running");
        assert_ne!(tasks[0]["id"], other);
    }

    #[tokio::test]
    async fn workers_lists_cards_with_a_worker_session() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "member").await;
        seed_session(&state, "w1", None, Some("p1")).await;
        card(
            &state,
            "c1",
            "todo",
            UpdateCard {
                worker_session_id: Some(Some("w1".into())),
                ..Default::default()
            },
        )
        .await;
        card(&state, "c2", "todo", UpdateCard::default()).await;
        state
            .db
            .open_card_run(
                "c1",
                "w1",
                crate::db::crud::CardRunStart {
                    step: "todo".into(),
                    role: "work".into(),
                    model: Some("mock:happy-path".into()),
                },
                "2026-10-10T00:00:00+00:00",
            )
            .await
            .unwrap();

        let (status, body) = get_json(&state, &token, "/api/dashboard/workers?project_id=p1").await;
        assert_eq!(status, StatusCode::OK);
        let workers = body["workers"].as_array().unwrap();
        assert_eq!(workers.len(), 1, "{body}");
        let w = &workers[0];
        assert_eq!(w["card_id"], "c1");
        assert_eq!(w["session_name"], "w1");
        assert_eq!(w["model"], "mock:happy-path");
        assert_eq!(w["started_at"], "2026-10-10T00:00:00+00:00");
        assert_eq!(w["running"], false);
        assert!(w["context_tokens"].is_null());

        let (status, _) = get_json(&state, &token, "/api/dashboard/workers?project_id=nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn worktrees_lists_unmerged_cards_and_skips_non_git_folders() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "member").await;
        seed_session(&state, "s", None, Some("p1")).await;
        card(
            &state,
            "c1",
            "done",
            UpdateCard {
                worktree_unmerged_reason: Some(Some("conflict".into())),
                worktree_unmerged_detail: Some(Some("CONFLICT in a.rs".into())),
                ..Default::default()
            },
        )
        .await;
        card(&state, "c2", "done", UpdateCard::default()).await;

        let (status, body) = get_json(&state, &token, "/api/dashboard/worktrees").await;
        assert_eq!(status, StatusCode::OK);
        let unmerged = body["unmerged"].as_array().unwrap();
        assert_eq!(unmerged.len(), 1, "{body}");
        assert_eq!(unmerged[0]["reason"], "conflict");
        assert_eq!(unmerged[0]["detail"], "CONFLICT in a.rs");
        // The seeded folder (/tmp/f1) is not a git repo.
        assert_eq!(body["commits"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn dependencies_returns_graph_and_top_blockers() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "member").await;
        seed_session(&state, "s", None, Some("p1")).await;
        for id in ["a", "b", "c", "lonely"] {
            card(&state, id, "todo", UpdateCard::default()).await;
        }
        state
            .db
            .set_card_dependencies("b", vec!["a".into()])
            .await
            .unwrap();
        state
            .db
            .set_card_dependencies("c", vec!["b".into()])
            .await
            .unwrap();

        let (status, _) = get_json(&state, &token, "/api/dashboard/dependencies").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, body) =
            get_json(&state, &token, "/api/dashboard/dependencies?project_id=p1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["nodes"].as_array().unwrap().len(), 3, "{body}");
        assert_eq!(body["edges"].as_array().unwrap().len(), 2);
        assert_eq!(body["top_blockers"][0]["card_id"], "a");
        assert_eq!(body["top_blockers"][0]["blocks"], 2);
        assert_eq!(body["top_blockers"][0]["title"], "title a");

        let (_, body) = get_json(
            &state,
            &token,
            "/api/dashboard/dependencies?project_id=p1&card_id=b",
        )
        .await;
        assert_eq!(body["nodes"].as_array().unwrap().len(), 2, "{body}");
        assert_eq!(body["top_blockers"][0]["card_id"], "a");
        assert_eq!(body["top_blockers"][0]["blocks"], 1);
    }
}
