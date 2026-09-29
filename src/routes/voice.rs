//! Voice assistant route: `POST /api/voice/session` gets or creates the
//! caller's single voice session (see `service::voice_relay`). Utterances go
//! through the ordinary `POST /api/sessions/:id/message` route and replies
//! stream over the normal WS — there is no voice-specific send path.

use std::sync::Arc;

use axum::{
    Extension, Json, Router, body::Bytes, extract::State, http::StatusCode, middleware,
    routing::post,
};
use serde::Deserialize;

use crate::auth::middleware::{AuthUser, require_auth};
use crate::db::models::{NewSession, Session, UpdateSession};
use crate::service::voice_relay::{VOICE_EXPERT_KIND, VOICE_SESSION_TITLE, VOICE_SYSTEM_PROMPT};
use crate::state::AppState;

type ApiError = (StatusCode, Json<serde_json::Value>);

#[derive(Deserialize, Default)]
struct VoiceSessionRequest {
    /// Switch the voice session to this model (provider-prefixed id).
    #[serde(default)]
    model: Option<String>,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/voice/session", post(voice_session))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// Serialises get-or-create so two concurrent first calls can't both
/// create a voice session for the same user.
static GET_OR_CREATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn err(status: StatusCode, msg: impl Into<String>) -> ApiError {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// POST /api/voice/session — body `{}` or `{"model": "<id>"}` →
/// `{"session_id", "model"}`. Idempotent.
async fn voice_session(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Empty body = `{}`; the frontend may post without one.
    let body: VoiceSessionRequest = if body.iter().all(u8::is_ascii_whitespace) {
        VoiceSessionRequest::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid body: {e}")))?
    };
    let requested = body
        .model
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    crate::routes::settings::check_model_or_400(requested.as_deref())?;

    let _guard = GET_OR_CREATE.lock().await;
    let existing = state
        .db
        .find_user_expert_session(&user.user_id, VOICE_EXPERT_KIND)
        .await
        .map_err(internal)?;

    let session = match existing {
        Some(session) => {
            // Keep the persona current across upgrades.
            if session.system_prompt.as_deref() != Some(VOICE_SYSTEM_PROMPT) {
                state
                    .db
                    .set_session_system_prompt(
                        &session.id,
                        Some(VOICE_SYSTEM_PROMPT.to_string()),
                        None,
                    )
                    .await
                    .map_err(internal)?;
            }
            match requested {
                Some(model) if session.model.as_deref() != Some(model.as_str()) => {
                    switch_model(&state, &session, &model).await?
                }
                _ => session,
            }
        }
        None => {
            let folder_id = pick_folder(&state, &user.user_id).await?;
            let model = match requested {
                Some(m) => Some(m),
                None => default_voice_model(&state).await,
            };
            let now = chrono::Utc::now().to_rfc3339();
            state
                .db
                .create_session(NewSession {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: VOICE_SESSION_TITLE.to_string(),
                    folder_id,
                    model,
                    created_at: now.clone(),
                    last_activity: now,
                    is_expert: true,
                    expert_kind: Some(VOICE_EXPERT_KIND.to_string()),
                    is_permanent: true,
                    system_prompt: Some(VOICE_SYSTEM_PROMPT.to_string()),
                    user_id: Some(user.user_id.clone()),
                    ..Default::default()
                })
                .await
                .map_err(internal)?
        }
    };

    let model = match session.model.clone() {
        Some(m) => m,
        None => crate::routes::settings::default_model_setting(&state)
            .await
            .unwrap_or_else(|| "default".to_string()),
    };
    Ok(Json(serde_json::json!({
        "session_id": session.id,
        "model": model,
    })))
}

/// Switch the voice session's model. Mirrors the session PATCH's forced
/// switch rather than its handover: a voice chat is short-lived small talk,
/// so across a provider/account boundary the new model simply starts cold
/// (no doc-generation turn on the outgoing model). A same-key switch keeps
/// `--resume`. The live child winds down after its turn either way, so the
/// next utterance spawns with the new model.
async fn switch_model(
    state: &Arc<AppState>,
    session: &Session,
    model: &str,
) -> Result<Session, ApiError> {
    let current = match session.model.clone() {
        Some(m) => m,
        None => crate::routes::settings::default_model_setting(state)
            .await
            .unwrap_or_else(|| "default".to_string()),
    };
    let cold = crate::handover::needs_handover(&current, model);
    let updated = state
        .db
        .update_session(
            &session.id,
            UpdateSession {
                model: Some(Some(model.to_string())),
                conversation_id: cold.then_some(None),
                handover_to_model: cold.then_some(None),
                pending_handover_doc: cold.then_some(None),
                handover_run_id: cold.then_some(None),
                ..Default::default()
            },
        )
        .await
        .map_err(internal)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "session not found"))?;
    crate::provider::manager::shutdown_after_turn_via_registry(
        &state.provider_registry,
        &session.id,
    )
    .await;
    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "session-updated".into(),
            session_id: updated.id.clone(),
            data: serde_json::to_value(&updated).unwrap_or(serde_json::Value::Null),
        });
    Ok(updated)
}

/// The folder a new voice session lives in: the one the user worked in most
/// recently, else the first folder. Sessions need a folder; the voice
/// assistant reaches other folders through the session tools.
async fn pick_folder(state: &Arc<AppState>, user_id: &str) -> Result<String, ApiError> {
    if let Some(s) = state
        .db
        .list_plain_sessions_page(Some(user_id), None, None, 1)
        .await
        .map_err(internal)?
        .into_iter()
        .next()
    {
        return Ok(s.folder_id);
    }
    state
        .db
        .list_folders()
        .await
        .map_err(internal)?
        .into_iter()
        .next()
        .map(|f| f.id)
        .ok_or_else(|| {
            err(
                StatusCode::CONFLICT,
                "create a folder first: the voice assistant session needs one",
            )
        })
}

/// A Haiku model the user can actually run (the same catalog the model
/// picker shows), preferring the account the app default bills to; else the
/// app default model; else `None` (dispatch-time default routing).
async fn default_voice_model(state: &Arc<AppState>) -> Option<String> {
    let hidden = crate::routes::settings::hidden_providers(state).await;
    let ids: Vec<String> = state
        .provider_registry
        .list_providers_with_models_except(&hidden)
        .await
        .into_iter()
        .filter(|p| p.id != "mock")
        .flat_map(|p| {
            p.models
                .into_iter()
                .map(move |m| format!("{}:{}", p.id, m.id))
        })
        .collect();
    let app_default = crate::routes::settings::default_model_setting(state).await;
    pick_haiku(&ids, app_default.as_deref()).or(app_default)
}

fn pick_haiku(ids: &[String], app_default: Option<&str>) -> Option<String> {
    let haikus: Vec<&String> = ids
        .iter()
        .filter(|id| id.to_ascii_lowercase().contains("haiku"))
        .collect();
    if let Some((_, account)) = app_default.and_then(|m| m.rsplit_once('@'))
        && let Some(id) = haikus
            .iter()
            .find(|id| id.rsplit_once('@').map(|(_, a)| a) == Some(account))
    {
        return Some((*id).clone());
    }
    haikus
        .iter()
        .find(|id| id.starts_with("claude:"))
        .or(haikus.first())
        .map(|id| (*id).clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::{seed_authenticated_user, test_state};
    use crate::db::models::NewFolder;
    use axum::body::Body;
    use axum::http::{Request, header};
    use tower::ServiceExt;

    #[test]
    fn pick_haiku_prefers_default_account() {
        let ids = vec![
            "claude:claude-opus-4-7@acc_a".to_string(),
            "claude:claude-haiku-4-5@acc_a".to_string(),
            "claude:claude-haiku-4-5@acc_b".to_string(),
        ];
        assert_eq!(
            pick_haiku(&ids, Some("claude:claude-opus-4-7@acc_b")).as_deref(),
            Some("claude:claude-haiku-4-5@acc_b")
        );
        assert_eq!(
            pick_haiku(&ids, None).as_deref(),
            Some("claude:claude-haiku-4-5@acc_a")
        );
        assert_eq!(pick_haiku(&ids[..1], None), None);
    }

    async fn post(app: &Router, token: &str, body: &str) -> (StatusCode, serde_json::Value) {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/voice/session")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn voice_session_is_idempotent_and_switches_model() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "user").await;
        state
            .db
            .create_folder(NewFolder {
                id: "f1".into(),
                name: "f".into(),
                path: dir.path().to_string_lossy().into(),
                created_at: "now".into(),
            })
            .await
            .unwrap();
        let app = router(state.clone()).with_state(state.clone());

        let (status, first) = post(&app, &token, "{}").await;
        assert_eq!(status, StatusCode::OK, "{first}");
        let sid = first["session_id"].as_str().unwrap().to_string();

        let (_, again) = post(&app, &token, "").await;
        assert_eq!(again["session_id"], first["session_id"]);

        let session = state.db.get_session(&sid).await.unwrap().unwrap();
        assert!(session.is_expert && session.is_permanent);
        assert_eq!(session.expert_kind.as_deref(), Some(VOICE_EXPERT_KIND));
        assert_eq!(session.name, VOICE_SESSION_TITLE);
        assert_eq!(session.user_id.as_deref(), Some("u1"));
        assert_eq!(session.system_prompt.as_deref(), Some(VOICE_SYSTEM_PROMPT));

        let (status, switched) = post(&app, &token, r#"{"model": "mock:happy-path"}"#).await;
        assert_eq!(status, StatusCode::OK, "{switched}");
        assert_eq!(switched["session_id"], first["session_id"]);
        assert_eq!(switched["model"], "mock:happy-path");
        assert_eq!(
            state
                .db
                .get_session(&sid)
                .await
                .unwrap()
                .unwrap()
                .model
                .as_deref(),
            Some("mock:happy-path")
        );
    }
}
