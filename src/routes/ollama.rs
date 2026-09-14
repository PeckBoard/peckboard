//! `/api/ollama/*` — management endpoints for the built-in Ollama
//! provider.
//!
//! * `POST /api/ollama/pull` — pull a model onto a configured Ollama
//!   server. Body: `{"model": "<name[@server]>"}` where the name is
//!   anything `ollama pull` accepts: a registry model (`llama3.2`,
//!   `qwen2.5-coder:7b`) or a Hugging Face GGUF repo
//!   (`hf.co/<user>/<repo>[:quant]`). A bare reference pulls onto the
//!   default `base_url` server; `@<alias>` targets one of the named
//!   Additional Servers. The response proxies Ollama's own streaming
//!   NDJSON progress (`{"status", "total", "completed"}` lines,
//!   terminated by `{"status":"success"}` or `{"error": …}`) so the UI
//!   can render live download progress. Once pulled, the model shows up
//!   in `/api/models` through the provider's normal autodiscovery.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Deserialize;

use crate::auth::middleware::{require_admin, require_auth};
use crate::plugin::settings::PluginSettingsStore;
use crate::state::AppState;

/// Pulling a model downloads onto a host-wide Ollama server (or writes to
/// its Additional Servers config) that every session shares, so it's
/// admin-only — same reasoning as `routes/settings.rs`.
pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/ollama/pull", post(pull_model))
        .route_layer(middleware::from_fn(require_admin))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

#[derive(Deserialize)]
struct PullBody {
    model: String,
}

/// POST /api/ollama/pull — start a pull and stream Ollama's NDJSON
/// progress back to the client. Errors before the pull starts (bad
/// reference, unconfigured server alias, unreachable server) come back
/// as plain JSON `{"error": …}`; errors mid-pull arrive as an `error`
/// line inside the NDJSON stream, exactly as Ollama reports them.
async fn pull_model(State(state): State<Arc<AppState>>, Json(body): Json<PullBody>) -> Response {
    let model = body.model.trim();
    if model.is_empty()
        || model.len() > 512
        || model.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return err(StatusCode::BAD_REQUEST, "invalid model reference");
    }

    let Some(schema) = state.plugins.settings_schema_for("ollama").await else {
        return err(StatusCode::NOT_FOUND, "ollama plugin is not available");
    };
    let store = PluginSettingsStore::new("ollama", schema, state.db.clone());
    let settings = match store.load().await {
        Ok(s) => s,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let resp = match start_pull(&settings, model).await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_GATEWAY, &e.to_string()),
    };

    let status = resp.status();
    if !status.is_success() {
        // Ollama reports failures as {"error": "..."}; surface the
        // message instead of a bare status code where we can.
        let text = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_else(|| format!("ollama returned HTTP {status}"));
        return err(StatusCode::BAD_GATEWAY, &msg);
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .body(Body::from_stream(resp.bytes_stream()))
        .unwrap_or_else(|_| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to build streaming response",
            )
        })
}

fn err(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

async fn start_pull(
    settings: &HashMap<String, serde_json::Value>,
    model_ref: &str,
) -> anyhow::Result<reqwest::Response> {
    let (name, alias) = match model_ref.rsplit_once('@') {
        Some((n, a)) if !n.is_empty() && !a.is_empty() => (n, Some(a)),
        _ => (model_ref, None),
    };
    let base_url = match alias {
        None => settings
            .get("base_url")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("ollama plugin: base_url is not configured"))?,
        Some(alias) => settings
            .get("servers")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .find_map(|entry| {
                let k = entry.get("key").and_then(|v| v.as_str())?;
                let v = entry.get("value").and_then(|v| v.as_str())?;
                (k == alias).then(|| v.to_string())
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "ollama plugin: model '{model_ref}' references server '{alias}', which \
                     is not configured under Additional Servers"
                )
            })?,
    };
    let trimmed = base_url.trim_end_matches('/');
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        anyhow::bail!("ollama base_url must be http or https");
    }
    let endpoint = format!("{trimmed}/api/pull");
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut req = client.post(&endpoint).json(&serde_json::json!({
        "model": name,
        "name": name,
        "stream": true,
    }));
    if let Some(arr) = settings
        .get("additional_headers")
        .and_then(|v| v.as_array())
    {
        for entry in arr {
            let Some(k) = entry.get("key").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(v) = entry.get("value").and_then(|v| v.as_str()) else {
                continue;
            };
            req = req.header(k, v);
        }
    }
    Ok(req.send().await?)
}
