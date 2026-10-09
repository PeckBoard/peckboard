//! `/api/terminals/*` — interactive SSH terminals ([`crate::terminal`]).
//!
//! Terminals are per user: list / rename / close act on the caller's own
//! (admins on any). Opening one — and listing the hosts it can open on — is
//! `require_admin`: fleet hosts carry shared, plugin-held credentials, so a
//! shell on one is an administrative capability. Responses never carry a
//! credential; the live shell is reached over `/ws/terminal/{id}`.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, patch},
};
use serde::{Deserialize, Serialize};

use crate::auth::middleware::{AuthUser, require_admin, require_auth};
use crate::db::models::Terminal;
use crate::state::AppState;
use crate::terminal::Status;

const NAME_MAX_LEN: usize = 128;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let admin = Router::new()
        .route("/api/terminals", axum::routing::post(create))
        .route("/api/terminals/hosts", get(list_hosts))
        .route_layer(middleware::from_fn(require_admin));
    let user = Router::new().route("/api/terminals", get(list)).route(
        "/api/terminals/{id}",
        patch(rename).get(get_one).delete(close),
    );
    admin
        .merge(user)
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

fn internal_err(e: impl std::fmt::Display) -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}

#[derive(Serialize)]
struct TerminalView {
    id: String,
    name: String,
    plugin_id: String,
    host_id: String,
    host_label: String,
    persistent: bool,
    created_at: String,
    last_active_at: String,
    status: Status,
}

fn view(state: &AppState, t: Terminal) -> TerminalView {
    let status = state.terminals.status_of(&t);
    TerminalView {
        persistent: status.persistent.unwrap_or(t.persistent),
        id: t.id,
        name: t.name,
        plugin_id: t.plugin_id,
        host_id: t.host_id,
        host_label: t.host_label,
        created_at: t.created_at,
        last_active_at: t.last_active_at,
        status,
    }
}

/// The caller's terminal, or `None` (404) when unknown, closed, or someone
/// else's — indistinguishable on purpose.
async fn owned(state: &AppState, user: &AuthUser, id: &str) -> Result<Terminal, Response> {
    match state.db.get_terminal(id).await {
        Ok(Some(t))
            if t.closed_at.is_none() && (t.user_id == user.user_id || user.role == "admin") =>
        {
            Ok(t)
        }
        Ok(_) => Err(err(StatusCode::NOT_FOUND, "terminal not found")),
        Err(e) => Err(internal_err(e)),
    }
}

async fn list(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
) -> Response {
    match state.db.list_terminals(Some(&user.user_id)).await {
        Ok(rows) => {
            let out: Vec<TerminalView> = rows.into_iter().map(|t| view(&state, t)).collect();
            Json(out).into_response()
        }
        Err(e) => internal_err(e),
    }
}

async fn get_one(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Response {
    match owned(&state, &user, &id).await {
        Ok(t) => Json(view(&state, t)).into_response(),
        Err(r) => r,
    }
}

async fn list_hosts(State(state): State<Arc<AppState>>) -> Response {
    Json(state.terminals.resolver().list_hosts().await).into_response()
}

#[derive(Deserialize)]
struct CreateBody {
    plugin_id: String,
    host_id: String,
    #[serde(default)]
    name: Option<String>,
}

async fn create(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<CreateBody>,
) -> Response {
    // The host must be one its plugin currently offers — also gives us the
    // display identity without touching a credential.
    let hosts = state.terminals.resolver().list_hosts().await;
    let Some(host) = hosts
        .into_iter()
        .find(|h| h.plugin_id == body.plugin_id && h.id == body.host_id)
    else {
        return err(StatusCode::NOT_FOUND, "host not found");
    };
    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&host.label)
        .chars()
        .take(NAME_MAX_LEN)
        .collect::<String>();
    let id = uuid::Uuid::new_v4().simple().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let row = Terminal {
        tmux_session: format!("peck-{id}"),
        id,
        user_id: user.user_id.clone(),
        plugin_id: host.plugin_id,
        host_id: host.id,
        name,
        host_label: format!("{}@{}:{}", host.username, host.hostname, host.port),
        persistent: false,
        created_at: now.clone(),
        last_active_at: now,
        closed_at: None,
    };
    match state.db.insert_terminal(row).await {
        Ok(t) => (StatusCode::CREATED, Json(view(&state, t))).into_response(),
        Err(e) => internal_err(e),
    }
}

#[derive(Deserialize)]
struct RenameBody {
    name: String,
}

async fn rename(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<RenameBody>,
) -> Response {
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > NAME_MAX_LEN {
        return err(StatusCode::BAD_REQUEST, "name must be 1-128 characters");
    }
    if let Err(r) = owned(&state, &user, &id).await {
        return r;
    }
    if let Err(e) = state.db.rename_terminal(&id, name).await {
        return internal_err(e);
    }
    match owned(&state, &user, &id).await {
        Ok(t) => Json(view(&state, t)).into_response(),
        Err(r) => r,
    }
}

/// Close for good: ends the remote shell (kills its tmux session), drops
/// its tabs, hides it from the list.
async fn close(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Response {
    let row = match owned(&state, &user, &id).await {
        Ok(t) => t,
        Err(r) => return r,
    };
    if let Err(e) = state.db.close_terminal(&id).await {
        return internal_err(e);
    }
    state.terminals.close(&row).await;
    (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
}
