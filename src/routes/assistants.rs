//! Named voice assistants (`service::assistants`). Admin-only, like the
//! voice session itself: an assistant can act on every session.
//!
//! - `GET    /api/assistants` → `{assistants: [Assistant + model], max, templates}`
//! - `POST   /api/assistants {name, job?, voice?, color?, icon?, template?}` → 201 assistant
//! - `PATCH  /api/assistants/{id} {name?, job?, voice?, color?, icon?, position?, is_default?}` → assistant
//! - `DELETE /api/assistants/{id}` → 204 (its conversation goes with it; the
//!   default assistant can't be deleted)
//!
//! An assistant's voice session is created by `POST /api/voice/session
//! {"assistant": id}` the first time it is opened.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, patch},
};
use serde::Deserialize;

use crate::auth::middleware::{require_admin, require_auth};
use crate::db::crud::assistants::{AssistantPatch, MAX_ASSISTANTS, NewAssistant};
use crate::db::models::{Assistant, UpdateSession};
use crate::service::assistants::TEMPLATES;
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/assistants", get(list).post(create))
        .route("/api/assistants/{id}", patch(update).delete(remove))
        .route_layer(middleware::from_fn(require_admin))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

fn err(status: StatusCode, msg: impl ToString) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
        .into_response()
}

fn internal(e: impl ToString) -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, e)
}

/// An assistant as the UI sees it: the row, the model its session runs,
/// how many updates are held for it (`waiting`: scheduled tasks that came
/// due, hand-offs, other sessions' news) and whether it is working on
/// something right now (`working`).
async fn to_json(state: &AppState, a: &Assistant) -> serde_json::Value {
    let mut model = None;
    let mut waiting = 0;
    let mut working = false;
    if let Some(sid) = a.session_id.as_deref() {
        model = state
            .db
            .get_session(sid)
            .await
            .ok()
            .flatten()
            .and_then(|s| s.model);
        waiting = crate::service::voice_gate::list(sid).len();
        let manager = &state.session_manager;
        working = manager.is_running(sid).await && !manager.is_lingering(sid).await;
    }
    let mut v = serde_json::to_value(a).unwrap_or_default();
    v["model"] = model.into();
    v["waiting"] = waiting.into();
    v["working"] = working.into();
    v
}

/// The roster changed (someone was added, renamed, given a new job or
/// removed): every assistant's prompt names the others, so bring them all
/// up to date. Each live agent picks it up after its current turn.
async fn refresh_prompts(state: &AppState) -> Result<(), Response> {
    crate::service::voice_prompt::apply_to_voice_session(&state.db, Some(&state.provider_registry))
        .await
        .map(|_| ())
        .map_err(internal)
}

async fn list(State(state): State<Arc<AppState>>) -> Response {
    let all = match state.db.list_assistants().await {
        Ok(all) => all,
        Err(e) => return internal(e),
    };
    let mut assistants = Vec::with_capacity(all.len());
    for a in &all {
        assistants.push(to_json(&state, a).await);
    }
    Json(serde_json::json!({
        "assistants": assistants,
        "max": MAX_ASSISTANTS,
        "templates": TEMPLATES,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    #[serde(default)]
    job: Option<String>,
    #[serde(default)]
    voice: Option<String>,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    template: Option<String>,
}

async fn create(State(state): State<Arc<AppState>>, Json(body): Json<CreateBody>) -> Response {
    let new = NewAssistant {
        name: body.name,
        job: body.job.unwrap_or_default(),
        voice: body.voice,
        color: body.color,
        icon: body.icon,
        template: body.template,
    };
    // Every failure here is about what was sent (name taken, too long, the
    // limit reached): the message says which.
    let created = match state.db.create_assistant(new).await {
        Ok(a) => a,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    if let Err(r) = refresh_prompts(&state).await {
        return r;
    }
    (StatusCode::CREATED, Json(to_json(&state, &created).await)).into_response()
}

/// Present-and-null clears a field; absent leaves it alone.
fn nullable<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

#[derive(Deserialize)]
struct UpdateBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    job: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    voice: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    color: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    icon: Option<Option<String>>,
    #[serde(default)]
    position: Option<i32>,
    /// Only `true` is meaningful: make this one the default.
    #[serde(default)]
    is_default: Option<bool>,
}

async fn update(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<UpdateBody>,
) -> Response {
    let patch = AssistantPatch {
        name: body.name,
        job: body.job,
        voice: body.voice,
        color: body.color,
        icon: body.icon,
        position: body.position,
    };
    let mut updated = match state.db.update_assistant(&id, patch).await {
        Ok(Some(a)) => a,
        Ok(None) => return err(StatusCode::NOT_FOUND, "no such assistant"),
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    if body.is_default == Some(true) && !updated.is_default {
        updated = match state.db.set_default_assistant(&id).await {
            Ok(Some(a)) => a,
            Ok(None) => return err(StatusCode::NOT_FOUND, "no such assistant"),
            Err(e) => return internal(e),
        };
    }
    // Its session carries its name in the sessions list.
    if let Some(sid) = updated.session_id.as_deref() {
        let rename = UpdateSession {
            name: Some(updated.name.clone()),
            ..Default::default()
        };
        if let Err(e) = state.db.update_session(sid, rename).await {
            return internal(e);
        }
    }
    if let Err(r) = refresh_prompts(&state).await {
        return r;
    }
    Json(to_json(&state, &updated).await).into_response()
}

async fn remove(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let removed = match state.db.delete_assistant(&id).await {
        Ok(Some(a)) => a,
        Ok(None) => return err(StatusCode::NOT_FOUND, "no such assistant"),
        // Only the default assistant is refused.
        Err(e) => return err(StatusCode::CONFLICT, e),
    };
    if let Some(sid) = removed.session_id.as_deref() {
        if let Err(e) = crate::routes::sessions::delete_session_core(&state, sid).await {
            return internal(e);
        }
    }
    if let Err(r) = refresh_prompts(&state).await {
        return r;
    }
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::{
        seed_authenticated_user, seed_authenticated_user_with_suffix, test_state,
    };
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn call(
        state: &Arc<AppState>,
        token: &str,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let app = router(state.clone())
            .merge(crate::routes::voice::router(state.clone()))
            .merge(crate::routes::repeating_tasks::router(state.clone()))
            .with_state(state.clone());
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(match body {
                Some(b) => Body::from(b.to_string()),
                None => Body::empty(),
            })
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    /// Two assistants get two separate voice sessions, each knows its own
    /// name and job and the other's; deleting one removes its session and
    /// drops it from the other's prompt.
    #[tokio::test]
    async fn assistants_have_their_own_sessions_and_know_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let admin = seed_authenticated_user(&state, "admin").await;
        let user = seed_authenticated_user_with_suffix(&state, "user", "b").await;
        state
            .db
            .create_folder(crate::db::models::NewFolder {
                id: "f".into(),
                name: "f".into(),
                path: dir.path().to_string_lossy().into_owned(),
                created_at: "now".into(),
            })
            .await
            .unwrap();

        let (s, _) = call(&state, &user, "GET", "/api/assistants", None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        // The default exists from the start and is the only one.
        let (s, body) = call(&state, &admin, "GET", "/api/assistants", None).await;
        assert_eq!(s, StatusCode::OK);
        let default_id = body["assistants"][0]["id"].as_str().unwrap().to_string();
        assert_eq!(body["assistants"].as_array().unwrap().len(), 1);
        assert_eq!(body["assistants"][0]["is_default"], true);
        assert!(!body["templates"].as_array().unwrap().is_empty());

        let (s, nova) = call(
            &state,
            &admin,
            "POST",
            "/api/assistants",
            Some(serde_json::json!({ "name": "Nova", "job": "Reviews pull requests." })),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED);
        let nova_id = nova["id"].as_str().unwrap().to_string();
        let (s, dup) = call(
            &state,
            &admin,
            "POST",
            "/api/assistants",
            Some(serde_json::json!({ "name": "nova" })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(dup["error"].as_str().unwrap().contains("already exists"));

        // Opening each one creates its own session.
        let open = |assistant: Option<String>| {
            let (state, admin) = (state.clone(), admin.clone());
            async move {
                let body = match assistant {
                    Some(a) => serde_json::json!({ "assistant": a, "model": "mock:happy-path" }),
                    None => serde_json::json!({ "model": "mock:happy-path" }),
                };
                call(&state, &admin, "POST", "/api/voice/session", Some(body)).await
            }
        };
        let (s, d) = open(None).await;
        assert_eq!(s, StatusCode::OK, "{d}");
        assert_eq!(d["assistant_id"], default_id.as_str());
        let (_, n) = open(Some(nova_id.clone())).await;
        let (default_sid, nova_sid) = (
            d["session_id"].as_str().unwrap().to_string(),
            n["session_id"].as_str().unwrap().to_string(),
        );
        assert_ne!(default_sid, nova_sid);
        let (_, again) = open(Some(nova_id.clone())).await;
        assert_eq!(again["session_id"], nova_sid.as_str());
        let (s, _) = open(Some("nope".into())).await;
        assert_eq!(s, StatusCode::NOT_FOUND);

        let prompt = |sid: String| {
            let state = state.clone();
            async move {
                state
                    .db
                    .get_session(&sid)
                    .await
                    .unwrap()
                    .unwrap()
                    .system_prompt
                    .unwrap_or_default()
            }
        };
        // Opening the default first stored its prompt before Nova had a
        // session; re-opening brings it up to date.
        open(None).await;
        let p = prompt(nova_sid.clone()).await;
        assert!(p.contains("Your name is Nova.") && p.contains("Reviews pull requests."));
        assert!(
            prompt(default_sid.clone())
                .await
                .contains("- Nova: Reviews pull requests.")
        );
        let nova_session = state.db.get_session(&nova_sid).await.unwrap().unwrap();
        assert_eq!(nova_session.name, "Nova");

        // Rename + new job reach both prompts and the session's name.
        let (s, renamed) = call(
            &state,
            &admin,
            "PATCH",
            &format!("/api/assistants/{nova_id}"),
            Some(serde_json::json!({ "name": "Max", "job": "Watches the servers." })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(renamed["model"], "mock:happy-path");
        assert!(prompt(nova_sid.clone()).await.contains("Your name is Max."));
        assert!(
            prompt(default_sid.clone())
                .await
                .contains("- Max: Watches the servers.")
        );
        let nova_session = state.db.get_session(&nova_sid).await.unwrap().unwrap();
        assert_eq!(nova_session.name, "Max");

        // The per-assistant routes answer for the assistant named.
        let (s, topics) = call(
            &state,
            &admin,
            "GET",
            &format!("/api/voice/topics?assistant={nova_id}"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(topics["topics"].as_array().unwrap().is_empty());

        // The default can't be deleted; the other one takes its session along.
        let (s, _) = call(
            &state,
            &admin,
            "DELETE",
            &format!("/api/assistants/{default_id}"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);
        let (s, _) = call(
            &state,
            &admin,
            "DELETE",
            &format!("/api/assistants/{nova_id}"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert!(state.db.get_session(&nova_sid).await.unwrap().is_none());
        assert!(!prompt(default_sid).await.contains("Max"));
    }

    /// A scheduled task set for one assistant is queued for that assistant
    /// when it runs; one that names no assistant goes to the default one.
    #[tokio::test]
    async fn a_scheduled_task_reaches_the_assistant_it_belongs_to() {
        use crate::service::voice_gate;
        let _gate = voice_gate::GATE_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let admin = seed_authenticated_user(&state, "admin").await;
        state
            .db
            .create_folder(crate::db::models::NewFolder {
                id: "f".into(),
                name: "f".into(),
                path: dir.path().to_string_lossy().into_owned(),
                created_at: "now".into(),
            })
            .await
            .unwrap();
        let post = |path: String, body: serde_json::Value| {
            let (state, admin) = (state.clone(), admin.clone());
            async move { call(&state, &admin, "POST", &path, Some(body)).await }
        };
        let session = serde_json::json!({ "model": "mock:happy-path" });
        let (_, d) = post("/api/voice/session".into(), session.clone()).await;
        let default_sid = d["session_id"].as_str().unwrap().to_string();
        let (s, nova) = post(
            "/api/assistants".into(),
            serde_json::json!({ "name": "Nova" }),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED);
        let nova_id = nova["id"].as_str().unwrap().to_string();
        let mut open_nova = session.clone();
        open_nova["assistant"] = nova_id.clone().into();
        let (_, n) = post("/api/voice/session".into(), open_nova).await;
        let nova_sid = n["session_id"].as_str().unwrap().to_string();

        let task = |assistant_id: Option<&str>, target: Option<&str>| {
            let mut body = serde_json::json!({
                "name": "check in",
                "folder_id": "f",
                "prompt": "Say what changed.",
                "schedule_kind": "daily",
                "schedule_value": { "hour": 9, "minute": 0 },
            });
            if let Some(t) = target {
                body["target"] = t.into();
            }
            if let Some(a) = assistant_id {
                body["assistant_id"] = a.into();
            }
            post("/api/repeating-tasks".into(), body)
        };

        // An assistant can only be named on an Assistant-target task, and
        // must exist.
        let (s, _) = task(Some(&nova_id), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _) = task(Some("nope"), Some("assistant")).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        let (s, novas) = task(Some(&nova_id), Some("assistant")).await;
        assert_eq!(s, StatusCode::CREATED, "{novas}");
        assert_eq!(novas["assistant_id"], nova_id.as_str());
        let (s, _) = post(
            format!("/api/repeating-tasks/{}/run", novas["id"].as_str().unwrap()),
            serde_json::json!({}),
        )
        .await;
        assert!(s.is_success(), "{s}");
        assert_eq!(voice_gate::list(&nova_sid).len(), 1);
        assert!(voice_gate::list(&default_sid).is_empty());
        // The list says so, which is what the switcher's badge shows.
        let (_, listed) = call(&state, &admin, "GET", "/api/assistants", None).await;
        let row = listed["assistants"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == nova_id.as_str())
            .unwrap()
            .clone();
        assert_eq!(row["waiting"], 1);
        assert_eq!(row["working"], false);

        let (s, plain) = task(None, Some("assistant")).await;
        assert_eq!(s, StatusCode::CREATED);
        assert!(plain["assistant_id"].is_null());
        let (s, _) = post(
            format!("/api/repeating-tasks/{}/run", plain["id"].as_str().unwrap()),
            serde_json::json!({}),
        )
        .await;
        assert!(s.is_success(), "{s}");
        assert_eq!(voice_gate::list(&default_sid).len(), 1);
        assert_eq!(voice_gate::list(&nova_sid).len(), 1);

        // A task whose assistant was deleted falls back to the default one.
        let (s, _) = call(
            &state,
            &admin,
            "DELETE",
            &format!("/api/assistants/{nova_id}"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (s, _) = post(
            format!("/api/repeating-tasks/{}/run", novas["id"].as_str().unwrap()),
            serde_json::json!({}),
        )
        .await;
        assert!(s.is_success(), "{s}");
        assert_eq!(voice_gate::list(&default_sid).len(), 2);
    }
}
