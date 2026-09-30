//! `/api/voice/lexicon*` — custom TTS pronunciations (see
//! `service::tts::lexicon`). Reads need a login; writes are admin-only, like
//! the voice session itself. Every write applies to the next sentence.
//!
//! - `GET    /api/voice/lexicon` → `[{word, display, respelling, phonemes, source, updated_at}]`
//! - `PUT    /api/voice/lexicon/{word} {display?, respelling?|phonemes?}` → entry, 400 `{error}`
//! - `DELETE /api/voice/lexicon/{word}` → 204 (a deleted default stays deleted)
//! - `POST   /api/voice/lexicon/preview {respelling?|phonemes?}` → `{phonemes}`, 400 `{error}`
//! - `GET    /api/voice/lexicon/unknown` → `[{word, count, first_seen, last_seen}]`
//! - `DELETE /api/voice/lexicon/unknown/{word}` → 204

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use serde::Deserialize;

use crate::auth::middleware::{require_admin, require_auth};
use crate::service::tts::lexicon::{self, LexiconError, LexiconStore};
use crate::state::AppState;

#[derive(Deserialize)]
struct PutBody {
    #[serde(default)]
    display: Option<String>,
    #[serde(default)]
    respelling: Option<String>,
    #[serde(default)]
    phonemes: Option<String>,
}

#[derive(Deserialize)]
struct PreviewBody {
    #[serde(default)]
    respelling: Option<String>,
    #[serde(default)]
    phonemes: Option<String>,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let reads = Router::new()
        .route("/api/voice/lexicon", get(list))
        .route("/api/voice/lexicon/unknown", get(list_unknown))
        .route("/api/voice/lexicon/preview", post(preview))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));
    let writes = Router::new()
        .route("/api/voice/lexicon/{word}", put(upsert).delete(remove))
        .route("/api/voice/lexicon/unknown/{word}", delete(dismiss))
        .route_layer(middleware::from_fn(require_admin))
        .route_layer(middleware::from_fn_with_state(state, require_auth));
    reads.merge(writes)
}

fn err(status: StatusCode, msg: impl ToString) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
        .into_response()
}

fn lex_err(e: LexiconError) -> Response {
    match e {
        LexiconError::Invalid(msg) => err(StatusCode::BAD_REQUEST, msg),
        LexiconError::Failed(e) => {
            tracing::warn!(error = %e, "tts lexicon write failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, e)
        }
    }
}

async fn store(state: &AppState) -> Result<Arc<LexiconStore>, Response> {
    lexicon::store(&state.db)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))
}

async fn list(State(state): State<Arc<AppState>>) -> Response {
    match store(&state).await {
        Ok(s) => Json(s.list()).into_response(),
        Err(r) => r,
    }
}

async fn upsert(
    State(state): State<Arc<AppState>>,
    Path(word): Path<String>,
    Json(body): Json<PutBody>,
) -> Response {
    let s = match store(&state).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match s
        .put(
            &word,
            body.display.as_deref(),
            body.respelling.as_deref(),
            body.phonemes.as_deref(),
        )
        .await
    {
        Ok(entry) => Json(entry).into_response(),
        Err(e) => lex_err(e),
    }
}

async fn remove(State(state): State<Arc<AppState>>, Path(word): Path<String>) -> Response {
    let s = match store(&state).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match s.delete(&word).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn preview(Json(body): Json<PreviewBody>) -> Response {
    let (resp, ph) = (body.respelling, body.phonemes);
    // Respelling runs misaki (CPU); keep it off the async workers.
    match tokio::task::spawn_blocking(move || {
        lexicon::resolve_phonemes(resp.as_deref(), ph.as_deref())
    })
    .await
    {
        Ok(Ok((_, phonemes))) => Json(serde_json::json!({ "phonemes": phonemes })).into_response(),
        Ok(Err(e)) => lex_err(e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn list_unknown(State(state): State<Arc<AppState>>) -> Response {
    let s = match store(&state).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match s.list_unknown().await {
        Ok(list) => Json(list).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn dismiss(State(state): State<Arc<AppState>>, Path(word): Path<String>) -> Response {
    let s = match store(&state).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match s.dismiss_unknown(&word).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::{seed_authenticated_user, test_state};
    use crate::service::tts::kokoro::phonemize_with;
    use crate::service::tts::lexicon::LexG2p;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn call(
        app: &Router,
        token: &str,
        method: &str,
        uri: &str,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    #[tokio::test]
    async fn lexicon_crud_preview_and_hot_reload() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;
        let app = router(state.clone()).with_state(state.clone());

        let (s, v) = call(&app, &token, "GET", "/api/voice/lexicon", "").await;
        assert_eq!(s, StatusCode::OK);
        let words: Vec<&str> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["display"].as_str().unwrap())
            .collect();
        assert!(words.contains(&"Peckboard") && words.contains(&"Kokoro"));
        let mut sorted = words.clone();
        sorted.sort_by_key(|w| w.to_lowercase());
        assert_eq!(words, sorted);

        let (s, v) = call(
            &app,
            &token,
            "POST",
            "/api/voice/lexicon/preview",
            r#"{"respelling":"koh-KOH-roh"}"#,
        )
        .await;
        assert_eq!(
            (s, v["phonemes"].as_str()),
            (StatusCode::OK, Some("kOkˈOɹO"))
        );

        let (s, v) = call(
            &app,
            &token,
            "PUT",
            "/api/voice/lexicon/Zorbl",
            r#"{"phonemes":"z#"}"#,
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains("'#'"));
        let (s, _) = call(&app, &token, "PUT", "/api/voice/lexicon/Zorbl", "{}").await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        let (s, v) = call(
            &app,
            &token,
            "PUT",
            "/api/voice/lexicon/Zorbl",
            r#"{"respelling":"ZOR-bull"}"#,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["word"], "zorbl");
        assert_eq!(v["display"], "Zorbl");
        assert_eq!(v["respelling"], "ZOR-bull");
        assert_eq!(v["source"], "user");
        // Hot reload: the very next phonemization uses it.
        let lex = lexicon::store(&state.db).await.unwrap();
        let ph = phonemize_with(&LexG2p::new(false), Some(&lex), "Zorbl.", false).unwrap();
        assert!(ph.starts_with(v["phonemes"].as_str().unwrap()), "{ph}");

        let (s, _) = call(&app, &token, "DELETE", "/api/voice/lexicon/zorbl", "").await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert!(lex.lookup("zorbl", false).is_none());

        // Unknown words: tallied, listed, dismissed.
        phonemize_with(&LexG2p::new(false), Some(&lex), "Qwxv qwxv.", false).unwrap();
        let (s, v) = call(&app, &token, "GET", "/api/voice/lexicon/unknown", "").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v[0]["word"], "qwxv");
        assert_eq!(v[0]["count"], 2);
        let (s, _) = call(
            &app,
            &token,
            "DELETE",
            "/api/voice/lexicon/unknown/qwxv",
            "",
        )
        .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (_, v) = call(&app, &token, "GET", "/api/voice/lexicon/unknown", "").await;
        assert!(v.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn writes_are_admin_only() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "user").await;
        let app = router(state.clone()).with_state(state);
        let (s, _) = call(&app, &token, "GET", "/api/voice/lexicon", "").await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = call(
            &app,
            &token,
            "PUT",
            "/api/voice/lexicon/Zorbl",
            r#"{"phonemes":"zˈɔɹbəl"}"#,
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }
}
