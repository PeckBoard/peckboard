//! `/api/skills*`, `/api/skill-groups*`, `/api/skill-sources*` and
//! `/api/sessions/{id}/skills` — the Skills feature's REST surface. Thin
//! wrappers over `service::skills`, which owns validation, ownership
//! (admin-or-owner on a skill; admin-only on shared groups / sources) and
//! the proprietary-content guard (422 `proprietary_content`), so the MCP
//! tools apply exactly the same rules.
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{get, post, put},
};
use serde::Deserialize;

use crate::auth::middleware::{AuthUser, require_auth, require_session_access};
use crate::service::skills::{self, Actor, SkillError, collab};
use crate::state::AppState;

type ApiError = (StatusCode, Json<serde_json::Value>);

fn api_err(e: SkillError) -> ApiError {
    if let SkillError::Internal(inner) = &e {
        tracing::warn!("skills route: {inner:#}");
    }
    (
        StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(e.body()),
    )
}

fn actor(user: &AuthUser) -> Actor {
    Actor {
        user_id: Some(user.user_id.clone()),
        is_admin: user.is_admin(),
    }
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    // The session-scoped pair rides the same ownership check as every other
    // `/api/sessions/{id}/*` route.
    let session_scoped = Router::new()
        .route(
            "/api/sessions/{id}/skills",
            get(get_session_skills).put(put_session_skills),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_session_access,
        ));
    Router::new()
        .route("/api/skills", get(list_skills).post(create_skill))
        .route("/api/skills/search", get(search_skills))
        .route("/api/skills/subscribe", post(subscribe))
        .route(
            "/api/skills/{id}",
            get(get_skill).put(update_skill).delete(delete_skill),
        )
        .route("/api/skills/{id}/versions/{version}", get(get_version))
        .route(
            "/api/skills/{id}/subscription",
            put(set_subscription).delete(unsubscribe),
        )
        .route(
            "/api/skills/hub/handle",
            get(hub_handle).put(hub_set_handle),
        )
        .route("/api/skills/hub/skills/{name}", get(hub_skill))
        .route("/api/skills/hub/skills/{name}/history", get(hub_history))
        .route("/api/skills/hub/skills/{name}/forks", get(hub_forks))
        .route("/api/skills/hub/skills/{name}/diff", get(hub_diff))
        .route("/api/skills/hub/fork", post(hub_fork))
        .route("/api/skills/hub/forks/{fork}/sync", post(hub_sync_fork))
        .route(
            "/api/skills/hub/merge-requests",
            get(hub_list_mrs).post(hub_create_mr),
        )
        .route("/api/skills/hub/merge-requests/check", post(hub_check_mrs))
        .route("/api/skills/hub/merge-requests/{id}", get(hub_get_mr))
        .route(
            "/api/skills/hub/merge-requests/{id}/comments",
            post(hub_comment_mr),
        )
        .route(
            "/api/skills/hub/merge-requests/{id}/merge",
            post(hub_merge_mr),
        )
        .route(
            "/api/skills/hub/merge-requests/{id}/close",
            post(hub_close_mr),
        )
        .route("/api/skills/{id}/publish", post(publish))
        .route("/api/skill-groups", get(list_groups).post(create_group))
        .route(
            "/api/skill-groups/{id}",
            put(update_group).delete(delete_group),
        )
        .route("/api/skill-sources", get(list_sources).post(create_source))
        .route(
            "/api/skill-sources/{id}",
            axum::routing::delete(delete_source),
        )
        .route("/api/skill-sources/{id}/sync", post(sync_source))
        .route(
            "/api/skill-sources/{id}/memory",
            get(list_memory).post(add_memory),
        )
        .route(
            "/api/skill-sources/{id}/memory/{slug}",
            get(get_memory).put(update_memory).delete(remove_memory),
        )
        .merge(session_scoped)
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

// ── Skills ──────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct ListQuery {
    q: Option<String>,
    scope: Option<String>,
    group: Option<String>,
}

async fn list_skills(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let items = skills::list(
        &state.db,
        q.q.as_deref(),
        q.scope.as_deref(),
        q.group.as_deref(),
    )
    .await
    .map_err(api_err)?;
    Ok(Json(serde_json::json!({ "skills": items })))
}

#[derive(Deserialize, Default)]
struct SearchQuery {
    q: Option<String>,
    source_id: Option<String>,
}

async fn search_skills(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SearchQuery>,
) -> Result<impl IntoResponse, ApiError> {
    skills::search(
        &state.db,
        &state.config.data_dir,
        q.q.as_deref(),
        q.source_id.as_deref(),
    )
    .await
    .map(Json)
    .map_err(api_err)
}

async fn create_skill(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<skills::CreateSkill>,
) -> Result<impl IntoResponse, ApiError> {
    let v = skills::create(&state.db, &actor(&user), body)
        .await
        .map_err(api_err)?;
    Ok((StatusCode::CREATED, Json(v)))
}

async fn get_skill(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    skills::get(&state.db, &id).await.map(Json).map_err(api_err)
}

async fn get_version(
    State(state): State<Arc<AppState>>,
    Path((id, version)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    skills::get_version(&state.db, &id, &version)
        .await
        .map(Json)
        .map_err(api_err)
}

async fn update_skill(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<skills::UpdateSkill>,
) -> Result<impl IntoResponse, ApiError> {
    skills::update(&state.db, &actor(&user), &id, body)
        .await
        .map(Json)
        .map_err(api_err)
}

async fn delete_skill(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    skills::delete(&state.db, &actor(&user), &id)
        .await
        .map_err(api_err)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SubscribeBody {
    source_id: String,
    name: String,
    #[serde(default)]
    version: Option<String>,
}

async fn subscribe(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<SubscribeBody>,
) -> Result<impl IntoResponse, ApiError> {
    let v = skills::subscribe(
        &state.db,
        &state.config.data_dir,
        &actor(&user),
        &body.source_id,
        &body.name,
        body.version,
    )
    .await
    .map_err(api_err)?;
    Ok((StatusCode::CREATED, Json(v)))
}

#[derive(Deserialize, Default)]
struct VersionBody {
    #[serde(default)]
    version: Option<String>,
}

async fn set_subscription(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<VersionBody>,
) -> Result<impl IntoResponse, ApiError> {
    skills::set_subscription(
        &state.db,
        &state.config.data_dir,
        &actor(&user),
        &id,
        body.version,
    )
    .await
    .map(Json)
    .map_err(api_err)
}

async fn unsubscribe(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    skills::unsubscribe(&state.db, &actor(&user), &id)
        .await
        .map_err(api_err)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct PublishBody {
    target_source_id: String,
}

async fn publish(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<PublishBody>,
) -> Result<impl IntoResponse, ApiError> {
    skills::publish(
        &state.db,
        &state.config.data_dir,
        &actor(&user),
        &id,
        &body.target_source_id,
    )
    .await
    .map(Json)
    .map_err(api_err)
}

// ── Public hub: handle, forks, merge requests, history ──────────────────

async fn hub_handle(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    collab::my_handle(&state.db, &state.config.data_dir)
        .await
        .map(Json)
        .map_err(api_err)
}

#[derive(Deserialize)]
struct HandleBody {
    handle: String,
}

async fn hub_set_handle(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<HandleBody>,
) -> Result<impl IntoResponse, ApiError> {
    collab::set_handle(
        &state.db,
        &state.config.data_dir,
        &actor(&user),
        &body.handle,
    )
    .await
    .map(Json)
    .map_err(api_err)
}

async fn hub_skill(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    collab::skill_detail(&state.db, &state.config.data_dir, &name)
        .await
        .map(Json)
        .map_err(api_err)
}

async fn hub_history(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    collab::history(&state.db, &name)
        .await
        .map(Json)
        .map_err(api_err)
}

async fn hub_forks(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    collab::forks(&state.db, &name)
        .await
        .map(Json)
        .map_err(api_err)
}

#[derive(Deserialize, Default)]
struct DiffQuery {
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
}

async fn hub_diff(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<DiffQuery>,
) -> Result<impl IntoResponse, ApiError> {
    collab::diff(&state.db, &name, &q.from, &q.to)
        .await
        .map(Json)
        .map_err(api_err)
}

#[derive(Deserialize)]
struct ForkBody {
    name: String,
}

async fn hub_fork(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<ForkBody>,
) -> Result<impl IntoResponse, ApiError> {
    let v = collab::fork(&state.db, &state.config.data_dir, &actor(&user), &body.name)
        .await
        .map_err(api_err)?;
    Ok((StatusCode::CREATED, Json(v)))
}

async fn hub_sync_fork(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(fork): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    collab::sync_fork(&state.db, &state.config.data_dir, &actor(&user), &fork)
        .await
        .map(Json)
        .map_err(api_err)
}

#[derive(Deserialize, Default)]
struct MrListQuery {
    #[serde(default)]
    role: String,
    #[serde(default)]
    state: String,
}

async fn hub_list_mrs(
    State(state): State<Arc<AppState>>,
    Query(q): Query<MrListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    collab::list_merge_requests(&state.db, &state.config.data_dir, &q.role, &q.state)
        .await
        .map(Json)
        .map_err(api_err)
}

#[derive(Deserialize)]
struct MrCreateBody {
    fork: String,
    title: String,
    #[serde(default)]
    description: String,
}

async fn hub_create_mr(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Json(b): Json<MrCreateBody>,
) -> Result<impl IntoResponse, ApiError> {
    let v = collab::create_merge_request(
        &state.db,
        &state.config.data_dir,
        &actor(&user),
        &b.fork,
        &b.title,
        &b.description,
    )
    .await
    .map_err(api_err)?;
    Ok((StatusCode::CREATED, Json(v)))
}

/// Run the incoming merge-request poll now (the Merge requests tab does on
/// open): `{open, new}`, plus the usual WS broadcast / push.
async fn hub_check_mrs(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    collab::check_incoming(
        &state.db,
        &state.config.data_dir,
        Some(&state.broadcaster),
        Some(&state.push_service),
    )
    .await
    .map(Json)
    .map_err(api_err)
}

async fn hub_get_mr(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
) -> Result<impl IntoResponse, ApiError> {
    collab::get_merge_request(&state.db, id)
        .await
        .map(Json)
        .map_err(api_err)
}

#[derive(Deserialize)]
struct CommentBody {
    body: String,
}

async fn hub_comment_mr(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<u64>,
    Json(b): Json<CommentBody>,
) -> Result<impl IntoResponse, ApiError> {
    let v = collab::comment_merge_request(
        &state.db,
        &state.config.data_dir,
        &actor(&user),
        id,
        &b.body,
    )
    .await
    .map_err(api_err)?;
    Ok((StatusCode::CREATED, Json(v)))
}

#[derive(Deserialize, Default)]
struct MergeBody {
    #[serde(default)]
    publish: bool,
    #[serde(default)]
    bump: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

async fn hub_merge_mr(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<u64>,
    Json(b): Json<MergeBody>,
) -> Result<impl IntoResponse, ApiError> {
    collab::merge_merge_request(
        &state.db,
        &state.config.data_dir,
        &actor(&user),
        id,
        b.publish,
        b.bump.as_deref(),
        b.version.as_deref(),
    )
    .await
    .map(Json)
    .map_err(api_err)
}

async fn hub_close_mr(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<u64>,
) -> Result<impl IntoResponse, ApiError> {
    collab::close_merge_request(&state.db, &state.config.data_dir, &actor(&user), id)
        .await
        .map(Json)
        .map_err(api_err)
}

// ── Groups ──────────────────────────────────────────────────────────────

async fn list_groups(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    let groups = skills::list_groups(&state.db).await.map_err(api_err)?;
    Ok(Json(serde_json::json!({ "groups": groups })))
}

async fn create_group(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<skills::GroupInput>,
) -> Result<impl IntoResponse, ApiError> {
    let g = skills::create_group(&state.db, &actor(&user), body)
        .await
        .map_err(api_err)?;
    Ok((StatusCode::CREATED, Json(g)))
}

async fn update_group(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<skills::GroupInput>,
) -> Result<impl IntoResponse, ApiError> {
    skills::update_group(&state.db, &actor(&user), &id, body)
        .await
        .map(Json)
        .map_err(api_err)
}

async fn delete_group(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    skills::delete_group(&state.db, &actor(&user), &id)
        .await
        .map_err(api_err)?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Sources ─────────────────────────────────────────────────────────────

async fn list_sources(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    let sources = skills::list_sources(&state.db).await.map_err(api_err)?;
    Ok(Json(serde_json::json!({ "sources": sources })))
}

async fn create_source(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<skills::SourceInput>,
) -> Result<impl IntoResponse, ApiError> {
    let s = skills::create_source(&state.db, &actor(&user), body)
        .await
        .map_err(api_err)?;
    Ok((StatusCode::CREATED, Json(s)))
}

async fn delete_source(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    skills::delete_source(&state.db, &state.config.data_dir, &actor(&user), &id)
        .await
        .map_err(api_err)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn sync_source(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    skills::sync_source(&state.db, &state.config.data_dir, &actor(&user), &id)
        .await
        .map(Json)
        .map_err(api_err)
}

// ── Team memory ─────────────────────────────────────────────────────────

async fn list_memory(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let items = skills::list_memory(&state.db, &state.config.data_dir, &id)
        .await
        .map_err(api_err)?;
    Ok(Json(serde_json::json!({ "memory": items })))
}

async fn get_memory(
    State(state): State<Arc<AppState>>,
    Path((id, slug)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    skills::get_memory(&state.db, &state.config.data_dir, &id, &slug)
        .await
        .map(Json)
        .map_err(api_err)
}

#[derive(Deserialize)]
struct AddMemoryBody {
    slug: String,
    #[serde(default)]
    title: Option<String>,
    body: String,
}

async fn add_memory(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(b): Json<AddMemoryBody>,
) -> Result<impl IntoResponse, ApiError> {
    let doc = skills::write_memory(
        &state.db,
        &state.config.data_dir,
        &id,
        &b.slug,
        b.title,
        b.body,
        true,
    )
    .await
    .map_err(api_err)?;
    Ok((StatusCode::CREATED, Json(doc)))
}

#[derive(Deserialize)]
struct UpdateMemoryBody {
    #[serde(default)]
    title: Option<String>,
    body: String,
}

async fn update_memory(
    State(state): State<Arc<AppState>>,
    Path((id, slug)): Path<(String, String)>,
    Json(b): Json<UpdateMemoryBody>,
) -> Result<impl IntoResponse, ApiError> {
    skills::write_memory(
        &state.db,
        &state.config.data_dir,
        &id,
        &slug,
        b.title,
        b.body,
        false,
    )
    .await
    .map(Json)
    .map_err(api_err)
}

async fn remove_memory(
    State(state): State<Arc<AppState>>,
    Path((id, slug)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    skills::remove_memory(&state.db, &state.config.data_dir, &id, &slug)
        .await
        .map_err(api_err)?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Session assignment ──────────────────────────────────────────────────

async fn get_session_skills(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    skills::session_skills(&state.db, &id)
        .await
        .map(Json)
        .map_err(api_err)
}

#[derive(Deserialize, Default)]
struct SessionSkillsBody {
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    groups: Vec<String>,
}

async fn put_session_skills(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<SessionSkillsBody>,
) -> Result<impl IntoResponse, ApiError> {
    skills::set_session_skills(&state.db, &id, body.skills, body.groups)
        .await
        .map(Json)
        .map_err(api_err)
}
