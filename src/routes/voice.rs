//! Voice assistant route: `POST /api/voice/session` gets or creates the
//! instance's single, global voice session (see `service::voice_relay`).
//! Utterances go through the ordinary `POST /api/sessions/:id/message` route
//! and replies stream over the normal WS — there is no voice-specific send
//! path.
//!
//! The voice session may act on every session in every folder, so it is an
//! admin surface: non-admins get 403.

use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::State,
    http::StatusCode,
    middleware,
    routing::{get, post},
};
use serde::Deserialize;

use crate::auth::middleware::{AuthUser, require_auth};
use crate::db::models::{NewSession, Session, UpdateSession};
use crate::service::voice_relay::{VOICE_EXPERT_KIND, VOICE_SESSION_TITLE};
use crate::state::AppState;

type ApiError = (StatusCode, Json<serde_json::Value>);

#[derive(Deserialize, Default)]
struct VoiceSessionRequest {
    /// Switch the voice session to this model (provider-prefixed id).
    #[serde(default)]
    model: Option<String>,
}
#[derive(Deserialize)]
struct VoiceActivityRequest {
    session_id: String,
    /// `speaking` | `idle` | `sent` | `tts_start` | `tts_end` | `panel_visible`.
    state: String,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/voice/session", post(voice_session))
        .route("/api/voice/activity", post(voice_activity))
        .route("/api/voice/linked", get(linked_activity))
        .route("/api/voice/queue", get(list_queue))
        .route("/api/voice/queue/next", post(release_queue_next))
        .route("/api/voice/snooze", post(snooze_question))
        .route("/api/voice/topics", get(list_topics))
        .route("/api/voice/topics/focus", post(focus_topic))
        .route(
            "/api/voice/settings",
            get(get_assistant_settings).put(put_assistant_settings),
        )
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

const ADMIN_ONLY: &str = "the Assistant is admin-only";

/// GET /api/voice/settings → the Assistant's admin settings
/// (`service::assistant_settings`).
async fn get_assistant_settings(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
) -> Result<Json<crate::service::assistant_settings::AssistantSettings>, ApiError> {
    if !user.is_admin() {
        return Err(err(StatusCode::FORBIDDEN, ADMIN_ONLY));
    }
    Ok(Json(
        crate::service::assistant_settings::load(&state.db).await,
    ))
}

/// PUT /api/voice/settings — replace the settings, then rebuild the voice
/// session's prompt (some settings change its rules; the live agent picks
/// them up after its current turn). → the saved settings.
async fn put_assistant_settings(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<crate::service::assistant_settings::AssistantSettings>,
) -> Result<Json<crate::service::assistant_settings::AssistantSettings>, ApiError> {
    if !user.is_admin() {
        return Err(err(StatusCode::FORBIDDEN, ADMIN_ONLY));
    }
    crate::service::assistant_settings::save(&state.db, &body)
        .await
        .map_err(internal)?;
    crate::service::voice_prompt::apply_to_voice_session(&state.db, Some(&state.provider_registry))
        .await
        .map_err(internal)?;
    Ok(Json(body))
}

/// GET /api/voice/linked → `{working, sessions: [{id, name, folder_id,
/// project_id, state}]}`: the sessions the Assistant handed work to
/// (`voice_relay` links) that are busy right now — `state` is `working`
/// (an agent turn is running) or `needs-answer` (a question is pending).
/// Idle linked sessions are left out.
async fn linked_activity(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !user.is_admin() {
        return Err(err(StatusCode::FORBIDDEN, ADMIN_ONLY));
    }
    let Some(voice_id) = voice_session_id(&state).await? else {
        return Ok(Json(serde_json::json!({ "working": 0, "sessions": [] })));
    };
    let mut sessions: Vec<(bool, Session)> = Vec::new();
    for id in crate::service::voice_relay::linked_targets(&voice_id) {
        let Some(s) = state.db.get_session(&id).await.map_err(internal)? else {
            continue;
        };
        let manager = &state.session_manager;
        let working = manager.is_running(&id).await && !manager.is_lingering(&id).await;
        let asking = !working
            && !crate::service::questions::pending_question_events(&state.db, &id)
                .await
                .map_err(internal)?
                .is_empty();
        if working || asking {
            sessions.push((working, s));
        }
    }
    sessions.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    let working = sessions.iter().filter(|(w, _)| *w).count();
    let sessions: Vec<serde_json::Value> = sessions
        .into_iter()
        .map(|(working, s)| {
            serde_json::json!({
                "id": s.id,
                "name": s.name,
                "folder_id": s.folder_id,
                "project_id": s.project_id,
                "state": if working { "working" } else { "needs-answer" },
            })
        })
        .collect();
    Ok(Json(
        serde_json::json!({ "working": working, "sessions": sessions }),
    ))
}

/// GET /api/voice/queue → `{count, items}`: the relays the gate is holding
/// for the Assistant (`voice_gate::queue_payload`). Changes are pushed as
/// `voice-queue` WS events on the voice session's stream.
async fn list_queue(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !user.is_admin() {
        return Err(err(StatusCode::FORBIDDEN, ADMIN_ONLY));
    }
    Ok(Json(match voice_session_id(&state).await? {
        Some(voice_id) => crate::service::voice_gate::queue_payload(&voice_id),
        None => serde_json::json!({ "count": 0, "items": [] }),
    }))
}

/// POST /api/voice/queue/next — the user tapped "N updates waiting": the
/// next topic group is delivered to the Assistant now, as `voice_queue
/// next` would hand it over. → the queue after it, plus `released`.
async fn release_queue_next(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use crate::service::voice_gate;
    if !user.is_admin() {
        return Err(err(StatusCode::FORBIDDEN, ADMIN_ONLY));
    }
    let voice_id = voice_session_id(&state)
        .await?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no Assistant session yet"))?;
    let dispatcher = crate::service::mcp_server::AppExpertDispatcher::new(state.clone());
    let released =
        voice_gate::release_next(&state.db, &state.broadcaster, Some(&dispatcher), &voice_id)
            .await
            .map_err(internal)?;
    let mut payload = voice_gate::queue_payload(&voice_id);
    payload["released"] = released.into();
    Ok(Json(payload))
}

#[derive(Deserialize)]
struct SnoozeRequest {
    session_id: String,
    question_id: String,
    minutes: i64,
}

/// POST /api/voice/snooze `{session_id, question_id, minutes}` — "Ask me
/// later" on a relayed question: a one-off Assistant task `minutes` from now
/// whose prompt re-raises the question (with the ids `answer_question`
/// needs), and the question leaves the held queue. → 201 `{task, unqueued}`;
/// 409 when the question is no longer pending.
async fn snooze_question(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<SnoozeRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    use crate::db::models::{NewRepeatingTask, RepeatingTask};
    use crate::service::voice_relay::sanitize_untrusted;
    if !user.is_admin() {
        return Err(err(StatusCode::FORBIDDEN, ADMIN_ONLY));
    }
    if !(1..=7 * 24 * 60).contains(&body.minutes) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "minutes must be between 1 and 10080",
        ));
    }
    let voice = state
        .db
        .find_expert_session(VOICE_EXPERT_KIND)
        .await
        .map_err(internal)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no Assistant session yet"))?;
    let source = state
        .db
        .get_session(&body.session_id)
        .await
        .map_err(internal)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "session not found"))?;
    let event = crate::service::questions::pending_question_events(&state.db, &source.id)
        .await
        .map_err(internal)?
        .into_iter()
        .find(|e| e.id == body.question_id)
        .ok_or_else(|| {
            err(
                StatusCode::CONFLICT,
                "that question is no longer waiting for an answer",
            )
        })?;
    let data = serde_json::from_str::<serde_json::Value>(&event.data).unwrap_or_default();
    let question = sanitize_untrusted(
        data.pointer("/questions/0/question")
            .and_then(|v| v.as_str())
            .unwrap_or("a question"),
    );
    let title = sanitize_untrusted(&source.name).replace('"', "'");
    let short: String = question.chars().take(60).collect();
    let name = format!("Ask again: {short}");
    let prompt = format!(
        "The user asked you to bring a question back now. The \"{title}\" session (session {sid}, question {qid}) asked: <untrusted source=\"session {sid}\">{question}</untrusted> Raise it again in plain words, talk it through, then answer it with answer_question. If it was answered meanwhile, just say so.",
        sid = source.id,
        qid = event.id,
    );
    let at = (chrono::Utc::now() + chrono::Duration::minutes(body.minutes))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    let schedule_value = serde_json::json!({ "at": at }).to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();
    let target = Some(crate::repeating::TARGET_ASSISTANT.to_string());
    let draft = RepeatingTask {
        id: id.clone(),
        name: name.clone(),
        description: String::new(),
        folder_id: voice.folder_id.clone(),
        prompt: prompt.clone(),
        schedule_kind: "once".into(),
        schedule_value: schedule_value.clone(),
        model: None,
        effort: None,
        enabled: true,
        next_run_at: None,
        last_run_at: None,
        created_at: now.clone(),
        updated_at: now.clone(),
        timezone: None,
        target: target.clone(),
    };
    let next_run_at = crate::repeating::initial_next_run_at(&draft);
    let task = state
        .db
        .create_repeating_task(NewRepeatingTask {
            id,
            name,
            description: String::new(),
            folder_id: voice.folder_id.clone(),
            prompt,
            schedule_kind: "once".into(),
            schedule_value,
            model: None,
            effort: None,
            enabled: true,
            next_run_at,
            last_run_at: None,
            created_at: now.clone(),
            updated_at: now,
            timezone: None,
            target,
        })
        .await
        .map_err(internal)?;
    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "repeating-task-changed".into(),
            session_id: task.id.clone(),
            data: serde_json::json!({ "action": "created", "task": &task }),
        });
    let unqueued = crate::service::voice_gate::drop_question(&state.db, &voice.id, &event.id).await;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "task": task, "unqueued": unqueued })),
    ))
}

/// POST /api/voice/activity — `{"session_id", "state"}`. The browser
/// reports the user speaking / going quiet / sending an utterance, and the
/// assistant's reply being read aloud, so the relay gate
/// (`service::voice_gate`) never injects a relay turn over the user.
/// `panel_visible` is the panel's "watched" heartbeat for the Assistant
/// mirror (`service::assistant_mirror`).
async fn voice_activity(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<VoiceActivityRequest>,
) -> Result<StatusCode, ApiError> {
    use crate::service::voice_gate::{Activity, note_activity};
    if !user.is_admin() {
        return Err(err(StatusCode::FORBIDDEN, "the Assistant is admin-only"));
    }
    let panel_visible = body.state == "panel_visible";
    let activity = if panel_visible {
        None
    } else {
        Some(Activity::parse(&body.state).ok_or_else(|| {
            err(
                StatusCode::BAD_REQUEST,
                format!("unknown state '{}'", body.state),
            )
        })?)
    };
    let is_voice = matches!(
        state.db.get_session(&body.session_id).await.map_err(internal)?,
        Some(s) if s.expert_kind.as_deref() == Some(VOICE_EXPERT_KIND)
    );
    if !is_voice {
        return Err(err(StatusCode::NOT_FOUND, "not a voice session"));
    }
    tracing::debug!(session_id = %body.session_id, state = %body.state, "voice activity");
    match activity {
        Some(activity) => note_activity(&body.session_id, activity),
        // Presence for the Assistant mirror: no email digest while watched.
        None => crate::service::assistant_mirror::AssistantMirror::of(&state)
            .await
            .note_panel_visible(),
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Serialises get-or-create so two concurrent first calls can't both
/// create the voice session.
static GET_OR_CREATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn err(status: StatusCode, msg: impl Into<String>) -> ApiError {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// The voice session's id, or `None` before it was first created.
async fn voice_session_id(state: &Arc<AppState>) -> Result<Option<String>, ApiError> {
    Ok(state
        .db
        .find_expert_session(VOICE_EXPERT_KIND)
        .await
        .map_err(internal)?
        .map(|s| s.id))
}

/// GET /api/voice/topics → `{currentId, topics: [AssistantTopic]}`: the
/// Assistant page's topic ring (`service::voice_topics::payload`), open
/// topics only, with live status for session / project topics. Empty
/// before the voice session exists.
async fn list_topics(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !user.is_admin() {
        return Err(err(StatusCode::FORBIDDEN, "the Assistant is admin-only"));
    }
    let Some(voice_id) = voice_session_id(&state).await? else {
        return Ok(Json(serde_json::json!({ "currentId": null, "topics": [] })));
    };
    crate::service::voice_topics::payload(&state.db, &voice_id)
        .await
        .map(Json)
        .map_err(internal)
}

#[derive(Deserialize)]
struct FocusTopicRequest {
    id: String,
}

/// POST /api/voice/topics/focus `{"id"}` — the user picked a topic on the
/// Assistant page. Makes it current (broadcast as `voice-topics`, the same
/// path a voice-driven `focus` takes), then tells the assistant "Let's talk
/// about <title>." through the ordinary session-message dispatch, so it
/// shows in the chat and the model continues there. → the new ring.
async fn focus_topic(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<FocusTopicRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use crate::service::{voice_gate, voice_topics};
    if !user.is_admin() {
        return Err(err(StatusCode::FORBIDDEN, "the Assistant is admin-only"));
    }
    let voice_id = voice_session_id(&state)
        .await?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no Assistant session yet"))?;
    let topic = state
        .db
        .focus_voice_topic(&voice_id, &body.id)
        .await
        .map_err(internal)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no open topic with that id"))?;
    voice_topics::focus_gate(&state.db, &voice_id, &topic).await;
    voice_topics::broadcast(&state.db, &state.broadcaster, &voice_id).await;
    // Started or queued behind a running turn: either way it is in the chat.
    let _sent = crate::routes::sessions::send_user_text(
        &state,
        &voice_id,
        user,
        voice_topics::focus_message(&topic.title),
    )
    .await?;
    // As the browser reports for a sent utterance: the assistant owes a
    // reply, so the relay gate holds relays until it has answered.
    voice_gate::note_activity(&voice_id, voice_gate::Activity::Sent);
    voice_topics::payload(&state.db, &voice_id)
        .await
        .map(Json)
        .map_err(internal)
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
    if !user.is_admin() {
        return Err(err(
            StatusCode::FORBIDDEN,
            "the Assistant controls every session, so it is admin-only",
        ));
    }
    crate::routes::settings::check_model_or_400(requested.as_deref())?;

    let _guard = GET_OR_CREATE.lock().await;
    let existing = state
        .db
        .find_expert_session(VOICE_EXPERT_KIND)
        .await
        .map_err(internal)?;

    let session = match existing {
        Some(session) => {
            // Keep the persona current: the active prompt (the built-in
            // default across upgrades, or the user's edited one).
            crate::service::voice_prompt::apply_to_voice_session(
                &state.db,
                Some(&state.provider_registry),
            )
            .await
            .map_err(internal)?;
            let session = state
                .db
                .get_session(&session.id)
                .await
                .map_err(internal)?
                .unwrap_or(session);
            match requested {
                Some(model) if session.model.as_deref() != Some(model.as_str()) => {
                    switch_model(&state, &session, &model).await?
                }
                _ => session,
            }
        }
        None => {
            let folder_id = pick_folder(&state, &user.user_id).await?;
            let prompt = crate::service::voice_prompt::session_prompt(&state.db)
                .await
                .map_err(internal)?;
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
                    system_prompt: Some(prompt),
                    user_id: Some(user.user_id.clone()),
                    ..Default::default()
                })
                .await
                .map_err(internal)?
        }
    };
    grant_cross_folder_control(&state, &session.id).await?;

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

/// Give the voice session a standing "Approve always" cross-folder grant
/// for the session-control tools (send_message, terminate_agent, …), so it
/// can drive sessions in every folder without a per-folder approval prompt.
/// The grant lives where both the plugin's own gate and the host's
/// `session_control_auth::authorize` read it. Idempotent.
async fn grant_cross_folder_control(
    state: &Arc<AppState>,
    session_id: &str,
) -> Result<(), ApiError> {
    use crate::plugin::session_control_auth::{grant_always, has_always};
    const PLUGIN_ID: &str = "session-control";
    let db = state.db.clone();
    let session_id = session_id.to_string();
    tokio::task::spawn_blocking(move || {
        if has_always(&db, PLUGIN_ID, &session_id)? {
            return Ok(());
        }
        grant_always(&db, PLUGIN_ID, &session_id)
    })
    .await
    .map_err(internal)?
    .map_err(internal)
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
                "create a folder first: the Assistant session needs one",
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
    use crate::auth::middleware::tests::{
        seed_authenticated_user, seed_authenticated_user_with_suffix, test_state,
    };
    use crate::db::models::NewFolder;
    use crate::service::voice_relay::VOICE_SYSTEM_PROMPT;
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
    async fn voice_session_is_global_admin_only_and_switches_model() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;
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
        // Standing cross-folder grant for the session-control tools.
        assert!(
            crate::plugin::session_control_auth::has_always(&state.db, "session-control", &sid)
                .unwrap()
        );

        // Global, not per-user: another admin gets the same session; a
        // non-admin is refused (the session controls every session).
        let admin2 = seed_authenticated_user_with_suffix(&state, "admin", "b").await;
        let (_, shared) = post(&app, &admin2, "{}").await;
        assert_eq!(shared["session_id"], first["session_id"]);
        let user = seed_authenticated_user_with_suffix(&state, "user", "c").await;
        let (status, _) = post(&app, &user, "{}").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(session.name, VOICE_SESSION_TITLE);
        assert_eq!(session.user_id.as_deref(), Some("u1"));
        let expected = crate::service::voice_prompt::with_builtin_rules(VOICE_SYSTEM_PROMPT);
        assert_eq!(session.system_prompt.as_deref(), Some(expected.as_str()));

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

    async fn call(
        app: &Router,
        token: &str,
        method: &str,
        uri: &str,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
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
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    #[tokio::test]
    async fn focus_topic_sends_one_user_message_and_broadcasts() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        crate::provider::test_double::register_mock_provider(&state.provider_registry).await;
        let token = seed_authenticated_user(&state, "admin").await;
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

        // No voice session yet: an empty ring, not an error.
        let (status, empty) = call(&app, &token, "GET", "/api/voice/topics", "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(empty["topics"], serde_json::json!([]));

        let (_, created) = post(&app, &token, r#"{"model": "mock:echo"}"#).await;
        let sid = created["session_id"].as_str().unwrap().to_string();
        let topic = state
            .db
            .add_voice_topic(&sid, "conversation", "Dinner plans", None, None)
            .await
            .unwrap();
        let mut rx = state.broadcaster.subscribe_all();

        let (status, ring) = call(
            &app,
            &token,
            "POST",
            "/api/voice/topics/focus",
            &serde_json::json!({ "id": topic.id }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{ring}");
        assert_eq!(ring["currentId"], topic.id.as_str());
        assert_eq!(ring["topics"][0]["title"], "Dinner plans");

        let users: Vec<_> = state
            .db
            .events_tail(&sid, 50)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "user")
            .collect();
        assert_eq!(users.len(), 1);
        assert!(
            users[0].data.contains("Let's talk about Dinner plans."),
            "{}",
            users[0].data
        );

        let mut topics_events = 0;
        while let Ok(ev) = rx.try_recv() {
            if ev.event_type == crate::service::voice_topics::VOICE_TOPICS_EVENT {
                assert_eq!(ev.session_id, sid);
                assert_eq!(ev.data["currentId"], topic.id.as_str());
                topics_events += 1;
            }
        }
        assert_eq!(topics_events, 1);

        let (_, listed) = call(&app, &token, "GET", "/api/voice/topics", "").await;
        assert_eq!(listed["currentId"], topic.id.as_str());
        let (status, _) = call(
            &app,
            &token,
            "POST",
            "/api/voice/topics/focus",
            r#"{"id": "nope"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn queue_activity_and_snooze_are_admin_only_and_release_held_relays() {
        use crate::service::voice_gate::{self, RelayKind};
        let _serial = voice_gate::GATE_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        crate::provider::test_double::register_mock_provider(&state.provider_registry).await;
        let token = seed_authenticated_user(&state, "admin").await;
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

        let user = seed_authenticated_user_with_suffix(&state, "user", "c").await;
        let snooze_body = r#"{"session_id": "x", "question_id": "y", "minutes": 10}"#;
        for (method, uri, body) in [
            ("GET", "/api/voice/queue", ""),
            ("GET", "/api/voice/linked", ""),
            ("POST", "/api/voice/queue/next", ""),
            ("POST", "/api/voice/snooze", snooze_body),
        ] {
            let (status, _) = call(&app, &user, method, uri, body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        }

        let (_, created) = post(&app, &token, r#"{"model": "mock:echo"}"#).await;
        let voice_id = created["session_id"].as_str().unwrap().to_string();
        let src = state
            .db
            .create_session(crate::db::models::NewSession {
                id: uuid::Uuid::new_v4().to_string(),
                name: "stashify dev".into(),
                folder_id: "f1".into(),
                created_at: "now".into(),
                last_activity: "now".into(),
                user_id: Some("u1".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        crate::service::voice_relay::link(&src.id, &voice_id);
        let q = state
            .db
            .append_event(
                &src.id,
                "question",
                serde_json::json!({"questions": [{"question": "Which DB?"}]}),
            )
            .await
            .unwrap();
        voice_gate::enqueue(
            &state.db,
            &voice_id,
            &src,
            RelayKind::Question,
            Some(&q.id),
            "[relay] question from stashify",
            "Which DB?",
        )
        .await;
        voice_gate::enqueue(
            &state.db,
            &voice_id,
            &src,
            RelayKind::Update,
            None,
            "[relay] update from stashify: done",
            "done",
        )
        .await;

        let (status, queue) = call(&app, &token, "GET", "/api/voice/queue", "").await;
        assert_eq!(status, StatusCode::OK, "{queue}");
        assert_eq!(queue["count"], 2);
        assert_eq!(queue["items"][0]["kind"], "question");
        assert_eq!(queue["items"][0]["source_name"], "stashify dev");
        assert_eq!(queue["items"][0]["question_id"], q.id.as_str());

        // Linked and waiting on the user: listed as needing an answer.
        let (status, activity) = call(&app, &token, "GET", "/api/voice/linked", "").await;
        assert_eq!(status, StatusCode::OK, "{activity}");
        assert_eq!(activity["sessions"][0]["name"], "stashify dev");
        assert_eq!(activity["sessions"][0]["state"], "needs-answer");

        // "Ask me later": a one-off Assistant task carrying the ids, and the
        // question leaves the held queue.
        let (status, snoozed) = call(
            &app,
            &token,
            "POST",
            "/api/voice/snooze",
            &serde_json::json!({ "session_id": src.id, "question_id": q.id, "minutes": 10 })
                .to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{snoozed}");
        assert_eq!(snoozed["unqueued"], true);
        assert_eq!(snoozed["task"]["target"], "assistant");
        assert_eq!(snoozed["task"]["schedule_kind"], "once");
        let prompt = snoozed["task"]["prompt"].as_str().unwrap();
        assert!(
            prompt.contains(&q.id) && prompt.contains(&src.id),
            "{prompt}"
        );
        assert!(snoozed["task"]["next_run_at"].is_string());

        // Tap the badge: the update is delivered to the Assistant now.
        let (status, next) = call(&app, &token, "POST", "/api/voice/queue/next", "").await;
        assert_eq!(status, StatusCode::OK, "{next}");
        assert_eq!(next["released"], 1);
        assert_eq!(next["count"], 0);
        let relayed = state
            .db
            .events_tail(&voice_id, 50)
            .await
            .unwrap()
            .into_iter()
            .any(|e| e.kind == "user" && e.data.contains("[relay] update from stashify: done"));
        assert!(relayed);
        assert!(state.db.list_voice_relays().await.unwrap().is_empty());
    }
}
