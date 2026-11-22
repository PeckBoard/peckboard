//! The demo project: a small board of scripted cards that shows a new user
//! how Peckboard works before they have any AI account.
//!
//! - `POST /api/demo-project` creates it (or returns the existing one),
//! - `GET /api/demo-project` returns it or `null`,
//! - `DELETE /api/demo-project` deletes it and its folder entry.
//!
//! Its workers run on the hidden [`DEMO_MODEL_ID`]: the mock crate's `demo`
//! scenario says the `Say: …` lines of each step's instructions, then runs
//! the step's ```mcp blocks (`finish_card`), so cards walk backlog → in
//! progress → review → done without touching a file. With the Mock plugin
//! off, boot registers the mock crate in demo-only mode, which refuses every
//! turn that isn't `mock:demo` inside a project [`is_demo_project`] accepts.
//!
//! A demo project is marked without a schema change: its name starts with
//! [`DEMO_NAME_PREFIX`] AND its context carries the [`DEMO_MARKER`] line.

use axum::{
    Json, Router, extract::State, http::StatusCode, middleware, response::IntoResponse,
    routing::post,
};
use std::sync::Arc;

use crate::auth::middleware::require_auth;
use crate::db::Db;
use crate::db::models::{NewCard, NewFolder, NewProject, Project};
use crate::provider::registry::DEMO_MODEL_ID;
use crate::state::AppState;

/// Every demo project's name starts with this.
pub const DEMO_NAME_PREFIX: &str = "Demo: ";
/// The demo project's name.
pub const DEMO_PROJECT_NAME: &str = "Demo: Plan a small website";
/// The line in a project's context that marks it as the demo.
pub const DEMO_MARKER: &str = "[peckboard-demo-project]";
/// The demo folder, under the data dir.
const DEMO_DIR: &str = "demo-project";
const DEMO_FOLDER_NAME: &str = "Demo project";
const DEMO_WORKFLOW: &str = "task";

/// Whether `project` is the demo project (name prefix + context marker).
pub fn is_demo_project(project: &Project) -> bool {
    project.name.starts_with(DEMO_NAME_PREFIX)
        && project.context.lines().any(|l| l.trim() == DEMO_MARKER)
}
/// Path of the demo folder (`<data_dir>/demo-project`).
pub fn demo_folder_path(data_dir: &std::path::Path) -> String {
    data_dir.join(DEMO_DIR).to_string_lossy().to_string()
}

/// Whether a folder entry is the demo's folder. It sits under the data dir
/// (skipping `reject_unsafe_path`), so it is hidden from folder listings and
/// refused as a place for real sessions: only the demo's helpers run there.
pub fn is_demo_folder(data_dir: &std::path::Path, folder_path: &str) -> bool {
    folder_path == demo_folder_path(data_dir)
}

/// Serialises create/delete so two quick clicks can't make two demos.
static DEMO_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const CARDS: &[(&str, &str)] = &[
    (
        "Pick a name and a goal for the website",
        "Decide what the site is called and the one thing a visitor should do there.",
    ),
    (
        "List the pages",
        "Write down the pages the site needs: home, about, and a contact page.",
    ),
    (
        "Write the home page text",
        "A short welcome, what the site offers, and a button to get in touch.",
    ),
    (
        "Choose colours and fonts",
        "Pick two colours and one font that fit the site's mood.",
    ),
];

fn mcp_block(tool: &str, args: serde_json::Value) -> String {
    format!(
        "```mcp\n{}\n```",
        serde_json::json!({ "tool": tool, "args": args })
    )
}

/// Per-step instructions: the plain-words lines the helper says, then the
/// tool call that moves the card on.
fn step_instructions() -> [(&'static str, String); 2] {
    [
        (
            "in_progress",
            format!(
                "Say: Reading the card to see what is needed.\n\
                 Say: Thinking about a few options.\n\
                 Say: Picking the best one and writing it down.\n\
                 Say: Finished — handing it over for a check.\n\n{}",
                mcp_block(
                    "finish_card",
                    serde_json::json!({ "summary": "Wrote up a simple plan for this card." }),
                )
            ),
        ),
        (
            "review",
            format!(
                "Say: Checking the work against the card.\n\
                 Say: Everything the card asked for is there.\n\
                 Say: Looks good — marking it done.\n\n{}",
                mcp_block(
                    "finish_card",
                    serde_json::json!({ "summary": "Checked the plan; it covers the card." }),
                )
            ),
        ),
    ]
}

/// The demo project, if one exists.
pub async fn find_demo_project(db: &Db) -> anyhow::Result<Option<Project>> {
    Ok(db.list_projects().await?.into_iter().find(is_demo_project))
}

fn internal(e: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": e.to_string() })),
    )
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/demo-project",
            post(create_demo).get(get_demo).delete(delete_demo),
        )
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// GET /api/demo-project → the demo project, or `null`.
async fn get_demo(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let project = find_demo_project(&state.db).await.map_err(internal)?;
    let body = match project {
        Some(p) => crate::worker::budget::project_json(&state.db, &p).await,
        None => serde_json::Value::Null,
    };
    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(body))
}

/// POST /api/demo-project → 201 with the new demo project, or 200 with the
/// existing one.
async fn create_demo(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let _guard = DEMO_LOCK.lock().await;
    if let Some(p) = find_demo_project(&state.db).await.map_err(internal)? {
        let body = crate::worker::budget::project_json(&state.db, &p).await;
        return Ok((StatusCode::OK, Json(body)));
    }
    let project = create_demo_project(&state).await.map_err(internal)?;
    let body = crate::worker::budget::project_json(&state.db, &project).await;
    Ok::<_, (StatusCode, Json<serde_json::Value>)>((StatusCode::CREATED, Json(body)))
}

/// The demo's folder entry: reuse the one already pointing at
/// `<data_dir>/demo-project`, else create the directory and the entry. Not
/// run through `reject_unsafe_path` (which refuses anything under the data
/// dir): the folder is empty and the demo helper never touches files.
async fn demo_folder_id(state: &AppState) -> anyhow::Result<String> {
    let path = state.config.data_dir.join(DEMO_DIR);
    std::fs::create_dir_all(&path)?;
    let path = path.to_string_lossy().to_string();
    if let Some(f) = state
        .db
        .list_folders()
        .await?
        .into_iter()
        .find(|f| f.path == path)
    {
        return Ok(f.id);
    }
    let folder = state
        .db
        .create_folder(NewFolder {
            id: uuid::Uuid::new_v4().to_string(),
            name: DEMO_FOLDER_NAME.to_string(),
            path,
            created_at: chrono::Utc::now().to_rfc3339(),
        })
        .await?;
    Ok(folder.id)
}

async fn create_demo_project(state: &AppState) -> anyhow::Result<Project> {
    let folder_id = demo_folder_id(state).await?;
    let now = chrono::Utc::now().to_rfc3339();
    let context = format!(
        "A demo project. The helpers here follow a script and need no AI \
         account; delete the demo whenever you like.\n{DEMO_MARKER}"
    );
    let project = state
        .db
        .create_project(NewProject {
            id: uuid::Uuid::new_v4().to_string(),
            name: DEMO_PROJECT_NAME.to_string(),
            context,
            folder_id,
            worker_count: 2,
            status: "active".to_string(),
            workflow: DEMO_WORKFLOW.to_string(),
            model: Some(DEMO_MODEL_ID.to_string()),
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: false,
            worker_communication: false,
            worktree_isolation: false,
            created_at: now.clone(),
            last_accessed_at: now.clone(),
            budget_usd_cents: None,
            budget_period: None,
        })
        .await?;
    for (step, text) in step_instructions() {
        state
            .db
            .upsert_project_workflow_instruction(&project.id, DEMO_WORKFLOW, step, &text)
            .await?;
    }
    for (i, (title, description)) in CARDS.iter().enumerate() {
        let card = state
            .db
            .create_card(NewCard {
                id: uuid::Uuid::new_v4().to_string(),
                project_id: project.id.clone(),
                title: title.to_string(),
                description: description.to_string(),
                step: "backlog".to_string(),
                priority: if i == 0 { 1 } else { 2 },
                workflow: DEMO_WORKFLOW.to_string(),
                model: None,
                effort: None,
                blocked: false,
                block_reason: None,
                created_at: now.clone(),
                updated_at: now.clone(),
                system_prompt_name: None,
            })
            .await?;
        let card_value = serde_json::to_value(&card).unwrap_or_default();
        state
            .broadcaster
            .broadcast(crate::ws::broadcaster::WsEvent {
                event_type: "card-update".into(),
                session_id: project.id.clone(),
                data: serde_json::json!({ "card": card_value }),
            });
    }
    tracing::info!(project_id = %project.id, "Created the demo project");
    Ok(project)
}

/// DELETE /api/demo-project → 204. Stops its workers, deletes the demo
/// project (cards, worker sessions, events) and its folder entry. 404 when
/// there is no demo. Never touches any other project.
async fn delete_demo(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let _guard = DEMO_LOCK.lock().await;
    let Some(project) = find_demo_project(&state.db).await.map_err(internal)? else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no demo project" })),
        ));
    };
    // Cancel in-flight helpers before their sessions are deleted.
    crate::routes::projects::pause_project_inner(&state, &project.id, None, "demo-delete")
        .await
        .map_err(internal)?;
    state
        .db
        .delete_project_cascade(&project.id)
        .await
        .map_err(internal)?;
    // The folder entry goes too, unless something else still uses it.
    if let Some(folder) = state
        .db
        .get_folder(&project.folder_id)
        .await
        .map_err(internal)?
        && folder.path == state.config.data_dir.join(DEMO_DIR).to_string_lossy()
    {
        let others = state
            .db
            .list_projects_by_folder(&folder.id)
            .await
            .map_err(internal)?;
        if others.is_empty() {
            if let Err(e) = state.db.delete_folder(&folder.id).await {
                tracing::warn!(folder_id = %folder.id, "Could not delete the demo folder entry: {e}");
            }
            // Only removes an empty directory.
            let _ = std::fs::remove_dir(&folder.path);
        }
    }
    tracing::info!(project_id = %project.id, "Deleted the demo project");
    Ok::<_, (StatusCode, Json<serde_json::Value>)>(StatusCode::NO_CONTENT)
}
