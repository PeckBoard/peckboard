//! `/api/teams*` — teams of PeckBoard servers. Thin wrappers over
//! `service::teams`, which owns every rule.
//!
//! Admin-only as a whole: joining a team, inviting a server or removing one
//! acts for this whole server (it signs with the server's identity key), so
//! none of it is partitioned per user — same reasoning as
//! `routes/settings.rs`.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{delete, get, post},
};
use serde::Deserialize;

use crate::auth::middleware::{require_admin, require_auth};
use crate::service::teams::{self, TeamError};
use crate::state::AppState;

type ApiError = (StatusCode, Json<serde_json::Value>);

fn api_err(e: TeamError) -> ApiError {
    (
        StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(e.body()),
    )
}

/// Tell every open Team page to reload (ids and counts only).
fn changed(state: &AppState) {
    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "team-update".into(),
            session_id: String::new(),
            data: serde_json::json!({}),
        });
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/teams", get(overview).post(create))
        .route("/api/teams/refresh", post(refresh_all))
        .route("/api/teams/join/preview", post(preview))
        .route("/api/teams/join", post(join))
        .route("/api/teams/{id}", get(get_team).delete(forget))
        .route("/api/teams/{id}/refresh", post(refresh))
        .route("/api/teams/{id}/rename", post(rename))
        .route("/api/teams/{id}/leave", post(leave))
        .route("/api/teams/{id}/members/role", post(set_role))
        .route("/api/teams/{id}/members/remove", post(remove_member))
        .route(
            "/api/teams/{id}/invites",
            get(list_invites).post(create_invite),
        )
        .route("/api/teams/{id}/invites/{invite}", delete(revoke_invite))
        .route("/api/teams/{id}/requests", get(list_requests))
        .route("/api/teams/{id}/requests/{request}/approve", post(approve))
        .route("/api/teams/{id}/requests/{request}/reject", post(reject))
        // Layers run outer-to-inner on the request: `require_auth` (added
        // last) runs first and puts `AuthUser` where `require_admin` reads it.
        .route_layer(middleware::from_fn(require_admin))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// GET /api/teams → this server's key and every team it is in, was in, or
/// is waiting to join. Local state only; `POST /api/teams/refresh` updates it.
async fn overview(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    teams::overview(&state.db, &state.config.data_dir)
        .await
        .map(Json)
        .map_err(api_err)
}

/// POST /api/teams/refresh → `{changed}`: check the team service for every team.
async fn refresh_all(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let n = teams::refresh_all(&state.db, &state.config.data_dir)
        .await
        .map_err(api_err)?;
    Ok::<_, ApiError>(Json(serde_json::json!({ "changed": n })))
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    /// This server's name in the member list.
    label: String,
}

/// POST /api/teams `{name, label}` → 201 + the new team; this server is its owner.
async fn create(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateBody>,
) -> impl IntoResponse {
    let team = teams::create(&state.db, &state.config.data_dir, &body.name, &body.label)
        .await
        .map_err(api_err)?;
    changed(&state);
    Ok::<_, ApiError>((StatusCode::CREATED, Json(team)))
}

#[derive(Deserialize)]
struct LinkBody {
    link: String,
    #[serde(default)]
    label: String,
}

/// POST /api/teams/join/preview `{link}` → `{team_id, name, members}`.
async fn preview(Json(body): Json<LinkBody>) -> impl IntoResponse {
    teams::preview_invite(&body.link)
        .await
        .map(Json)
        .map_err(api_err)
}

/// POST /api/teams/join `{link, label}` → 201 + the pending team, with the
/// verification code to read out to whoever sent the invite.
async fn join(State(state): State<Arc<AppState>>, Json(body): Json<LinkBody>) -> impl IntoResponse {
    let team = teams::request_join(&state.db, &state.config.data_dir, &body.link, &body.label)
        .await
        .map_err(api_err)?;
    changed(&state);
    Ok::<_, ApiError>((StatusCode::CREATED, Json(team)))
}

async fn get_team(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    teams::get(&state.db, &state.config.data_dir, &id)
        .await
        .map(Json)
        .map_err(api_err)
}

/// DELETE /api/teams/:id → 204: drop a team this server is no longer in (or
/// a pending join, whose request is also withdrawn) from its list.
async fn forget(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    teams::forget(&state.db, &state.config.data_dir, &id)
        .await
        .map_err(api_err)?;
    changed(&state);
    Ok::<_, ApiError>(StatusCode::NO_CONTENT)
}

/// POST /api/teams/:id/refresh → the team after asking the team service.
/// Other open pages are told only when something actually changed: a page
/// waiting on a join polls this.
async fn refresh(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    let dir = &state.config.data_dir;
    let before = teams::get(&state.db, dir, &id).await.ok();
    let out = teams::refresh(&state.db, dir, &id).await;
    let after = teams::get(&state.db, dir, &id).await.ok();
    let fingerprint = |t: &teams::TeamView| {
        (
            t.state.clone(),
            t.role.clone(),
            t.name.clone(),
            t.members.len(),
            t.last_error.clone(),
        )
    };
    if before.as_ref().map(fingerprint) != after.as_ref().map(fingerprint) {
        changed(&state);
    }
    out.map(Json).map_err(api_err)
}

#[derive(Deserialize)]
struct RenameBody {
    name: String,
}

async fn rename(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<RenameBody>,
) -> impl IntoResponse {
    let team = teams::rename(&state.db, &state.config.data_dir, &id, &body.name)
        .await
        .map_err(api_err)?;
    changed(&state);
    Ok::<_, ApiError>(Json(team))
}

async fn leave(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    let team = teams::leave(&state.db, &state.config.data_dir, &id)
        .await
        .map_err(api_err)?;
    changed(&state);
    Ok::<_, ApiError>(Json(team))
}

#[derive(Deserialize)]
struct MemberBody {
    /// The member server's key (it does not fit a URL path: base64).
    key: String,
    #[serde(default)]
    role: String,
}

async fn set_role(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<MemberBody>,
) -> impl IntoResponse {
    let team = teams::set_role(
        &state.db,
        &state.config.data_dir,
        &id,
        &body.key,
        &body.role,
    )
    .await
    .map_err(api_err)?;
    changed(&state);
    Ok::<_, ApiError>(Json(team))
}

async fn remove_member(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<MemberBody>,
) -> impl IntoResponse {
    let team = teams::remove_member(&state.db, &state.config.data_dir, &id, &body.key)
        .await
        .map_err(api_err)?;
    changed(&state);
    Ok::<_, ApiError>(Json(team))
}

#[derive(Deserialize, Default)]
struct InviteBody {
    /// Days the invite stays valid (1 to 7; default 3).
    #[serde(default)]
    days: Option<i64>,
}

async fn list_invites(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    teams::list_invites(&state.db, &id)
        .await
        .map(Json)
        .map_err(api_err)
}

/// POST /api/teams/:id/invites `{days?}` → 201 `{invite_id, link, expires_at}`.
async fn create_invite(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<InviteBody>>,
) -> impl IntoResponse {
    let days = body.and_then(|b| b.0.days);
    let invite = teams::create_invite(&state.db, &state.config.data_dir, &id, days)
        .await
        .map_err(api_err)?;
    Ok::<_, ApiError>((StatusCode::CREATED, Json(invite)))
}

async fn revoke_invite(
    State(state): State<Arc<AppState>>,
    Path((id, invite)): Path<(String, String)>,
) -> impl IntoResponse {
    teams::revoke_invite(&state.db, &state.config.data_dir, &id, &invite)
        .await
        .map_err(api_err)?;
    Ok::<_, ApiError>(StatusCode::NO_CONTENT)
}

/// GET /api/teams/:id/requests → `{requests: [{request_id, label, key,
/// fingerprint, created_at, can_approve, genuine, code}]}`.
async fn list_requests(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    teams::list_requests(&state.db, &state.config.data_dir, &id)
        .await
        .map(Json)
        .map_err(api_err)
}

async fn approve(
    State(state): State<Arc<AppState>>,
    Path((id, request)): Path<(String, String)>,
) -> impl IntoResponse {
    let team = teams::approve_request(&state.db, &state.config.data_dir, &id, &request)
        .await
        .map_err(api_err)?;
    changed(&state);
    Ok::<_, ApiError>(Json(team))
}

async fn reject(
    State(state): State<Arc<AppState>>,
    Path((id, request)): Path<(String, String)>,
) -> impl IntoResponse {
    teams::reject_request(&state.db, &state.config.data_dir, &id, &request)
        .await
        .map_err(api_err)?;
    changed(&state);
    Ok::<_, ApiError>(StatusCode::NO_CONTENT)
}
