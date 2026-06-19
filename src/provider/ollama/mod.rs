//! Ollama agent provider.
//!
//! Hits Ollama's HTTP `/api/chat` endpoint in streaming mode and
//! translates each `{"message":{"content":"…"}}` chunk into a
//! [`ProviderEvent::Text`]. The provider is intentionally minimal —
//! Ollama doesn't expose tools, conversation IDs, or permission prompts,
//! so the entire stream is `Started → Text* → Completed | Crashed`.
//!
//! Multi-turn context is held in memory per session: each turn appends
//! the user message + assistant reply to the session's transcript and
//! the full transcript is replayed to Ollama on the next turn. The
//! Ollama API itself is stateless, so this is the simplest way to give
//! a conversation feel without involving the database. State is dropped
//! on cancel (so a `/clear` restart begins fresh) and on shutdown.
//!
//! Configuration (base URL, default model, request timeout, optional
//! HTTP headers) lives on the per-plugin [`PluginSettingsStore`] — the
//! `OllamaPlugin` constructs the store from its schema, then hands the
//! handle to this provider. The provider re-reads settings on every
//! `send_message` so a UI change takes effect immediately, without
//! requiring a restart.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::plugin::settings::PluginSettingsStore;
use crate::provider::agent::{AgentProvider, ProcessCompletion, SendMessageContext, emit_event};
use crate::provider::stream::{ModelInfo, ProviderEvent};

/// Default request timeout used when the user hasn't set one. Generous
/// because Ollama on CPU can take a while to load a fresh model.
const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// Hard upper bound on request timeout regardless of the configured
/// value. Stops a misconfigured setting from leaking a TCP connection
/// to a wedged Ollama instance for hours.
const MAX_TIMEOUT_SECS: u64 = 3600;

/// Most-recent messages kept in per-session history. Past this point
/// each new turn would otherwise grow the in-memory buffer (and the
/// outbound request body Ollama replays back through) without bound.
/// 50 user/assistant pairs ≈ a long working conversation.
const MAX_HISTORY_MESSAGES: usize = 100;

/// How long a `/v1/models` discovery result (success *or* failure) is
/// reused before the provider probes the server again. `dynamic_models`
/// is on the read-only catalog path (`/api/models`, the model picker),
/// which can fire several times per page; without a cache every one of
/// those would block on a network round-trip to Ollama. Caching the
/// failure case too means a down/remote server adds its connect cost at
/// most once per window instead of to every catalog read.
const MODEL_DISCOVERY_TTL: Duration = Duration::from_secs(30);

/// Per-request timeout for the discovery call. Much tighter than the
/// chat timeout — listing models is cheap, and a slow answer here
/// shouldn't stall the whole model picker.
const MODEL_DISCOVERY_TIMEOUT_SECS: u64 = 15;

/// One in-flight `send_message` per session. The `cancel` notify is used
/// by `cancel`/`interrupt` to wind the stream down cleanly so the
/// orchestrator still gets its `ProcessCompletion`.
struct OllamaRun {
    handle: JoinHandle<()>,
    cancel: Arc<Notify>,
}

#[derive(Clone, Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
}

#[derive(Deserialize)]
struct ChatStreamChunk {
    #[serde(default)]
    message: Option<StreamMessage>,
    #[serde(default)]
    done: bool,
    /// Present when Ollama itself returns an error mid-stream (e.g.
    /// "model not found"). Surfaced as a Crashed event so the user sees
    /// the cause in the UI instead of a silent hang.
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct StreamMessage {
    #[serde(default)]
    content: Option<String>,
}

/// Shape of Ollama's OpenAI-compatible `GET /v1/models` response. We only
/// need the model ids (`data[].id`, e.g. `llama3.1:8b`); the rest of the
/// OpenAI envelope (`object`, `created`, `owned_by`) is ignored.
#[derive(Deserialize)]
struct OpenAiModelsResponse {
    #[serde(default)]
    data: Vec<OpenAiModel>,
}

#[derive(Deserialize)]
struct OpenAiModel {
    #[serde(default)]
    id: String,
}

/// Cached outcome of the last `/v1/models` discovery probe. `models` is
/// `Some` on success (possibly an empty list when the server has nothing
/// pulled) and `None` when the last attempt failed — cached either way so
/// a wedged server isn't re-probed on every catalog read (see
/// [`MODEL_DISCOVERY_TTL`]).
struct DiscoveryCache {
    fetched_at: Instant,
    models: Option<Vec<String>>,
}

pub struct OllamaProvider {
    settings: PluginSettingsStore,
    runs: Arc<Mutex<HashMap<String, OllamaRun>>>,
    /// Per-session multi-turn history. Ollama is stateless, so the
    /// provider has to replay the whole transcript on every turn. Dropped
    /// on cancel and shutdown.
    conversations: Arc<Mutex<HashMap<String, Vec<ChatMessage>>>>,
    /// TTL cache for the `/v1/models` autodiscovery probe so the model
    /// picker doesn't trigger a network round-trip on every render.
    discovery_cache: Arc<Mutex<Option<DiscoveryCache>>>,
    client: reqwest::Client,
}

impl OllamaProvider {
    pub fn new(settings: PluginSettingsStore) -> Self {
        OllamaProvider {
            settings,
            runs: Arc::new(Mutex::new(HashMap::new())),
            conversations: Arc::new(Mutex::new(HashMap::new())),
            discovery_cache: Arc::new(Mutex::new(None)),
            // `redirect(Policy::none())` is load-bearing: an attacker
            // who controls `base_url` could otherwise 302 us to e.g.
            // `http://169.254.169.254/latest/meta-data/iam/security-credentials`
            // (cloud metadata) and have the response surface into the
            // event log. `connect_timeout` bounds the per-attempt
            // wait when the configured host doesn't TCP-accept —
            // without it a wedged LAN target consumes a worker slot
            // for the full request timeout.
            client: reqwest::Client::builder()
                .pool_idle_timeout(Some(Duration::from_secs(60)))
                .connect_timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest client builds with default config"),
        }
    }

    /// Return the model ids installed on the server, going through the
    /// TTL cache so the picker doesn't probe Ollama on every render.
    /// `Some(list)` on success (possibly empty), `None` when the last
    /// probe failed and the caller should fall back to the static seed.
    async fn discovered_models(
        &self,
        settings: &HashMap<String, serde_json::Value>,
    ) -> Option<Vec<String>> {
        {
            let cache = self.discovery_cache.lock().await;
            if let Some(entry) = cache.as_ref()
                && entry.fetched_at.elapsed() < MODEL_DISCOVERY_TTL
            {
                return entry.models.clone();
            }
        }
        let result = self.fetch_server_models(settings).await;
        let mut cache = self.discovery_cache.lock().await;
        *cache = Some(DiscoveryCache {
            fetched_at: Instant::now(),
            models: result.clone(),
        });
        result
    }

    /// Hit `<base_url>/v1/models` and return the parsed model ids. Returns
    /// `None` (so the caller falls back to the static seed) on any
    /// failure: no `base_url`, a bad URL, a network/HTTP error, or an
    /// unparseable body. An empty-but-valid response is `Some(vec![])`.
    async fn fetch_server_models(
        &self,
        settings: &HashMap<String, serde_json::Value>,
    ) -> Option<Vec<String>> {
        let base_url = setting_str(settings, "base_url")?;
        let endpoint = match build_endpoint(&base_url, "/v1/models") {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("ollama: bad base_url for model discovery: {e}");
                return None;
            }
        };

        let mut request = self
            .client
            .get(&endpoint)
            .timeout(Duration::from_secs(MODEL_DISCOVERY_TIMEOUT_SECS));
        // Same auth-proxy headers the chat path uses (Ollama ignores the
        // OpenAI `Authorization` itself, but a fronting proxy may not).
        for (name, value) in setting_headers(settings, "additional_headers") {
            match (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_str(&value),
            ) {
                (Ok(n), Ok(v)) => request = request.header(n, v),
                _ => tracing::warn!(header = %name, "Dropping malformed Ollama header"),
            }
        }

        let response = match request.send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("ollama: model discovery request failed: {e}");
                return None;
            }
        };
        if !response.status().is_success() {
            tracing::warn!(
                "ollama: model discovery returned HTTP {}",
                response.status()
            );
            return None;
        }
        let body = match response.text().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("ollama: failed to read model discovery body: {e}");
                return None;
            }
        };
        parse_openai_models(&body)
    }
}

#[async_trait]
impl AgentProvider for OllamaProvider {
    fn id(&self) -> &str {
        "ollama"
    }

    async fn dynamic_models(&self) -> Option<Vec<ModelInfo>> {
        // The catalog the picker shows is, in order of preference:
        //   1. the models actually installed on the server (autodiscovery
        //      via the OpenAI-compatible `/v1/models` endpoint), or
        //   2. the built-in static seed, when discovery is off or the
        //      server is unreachable,
        // with the user's manually-registered `additional_models` merged
        // on top either way. Re-read on every catalog request so a
        // settings edit shows up without a restart; on a settings-load
        // error fall back to the static seed rather than dropping the
        // provider's models entirely.
        let settings = match self.settings.load().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("ollama: failed to load settings for model list: {e}");
                return Some(default_models());
            }
        };

        let extras = setting_str_list(&settings, "additional_models");
        let discover = setting_bool(&settings, "discover_models").unwrap_or(true);

        let base = if discover {
            match self.discovered_models(&settings).await {
                Some(names) => names.into_iter().map(model_info).collect(),
                // Discovery failed: keep the static suggestions so the
                // picker isn't empty while the server is unreachable.
                None => default_models(),
            }
        } else {
            default_models()
        };

        Some(merge_additional_models(base, extras))
    }

    async fn send_message(&self, ctx: SendMessageContext) -> anyhow::Result<()> {
        // Ollama doesn't drive plugin-todos today, so the plugin host
        // is intentionally ignored.
        let SendMessageContext {
            session_id,
            message,
            db,
            broadcaster,
            config,
            conversation_id: _,
            completion_tx,
            plugins: _,
        } = ctx;

        // Wind down any prior run on this session before starting a new one.
        {
            let mut runs = self.runs.lock().await;
            if let Some(old) = runs.remove(&session_id) {
                old.cancel.notify_one();
            }
        }

        let settings = self.settings.load().await?;
        let base_url = setting_str(&settings, "base_url")
            .ok_or_else(|| anyhow::anyhow!("ollama plugin: base_url is not configured"))?;
        let default_model = setting_str(&settings, "default_model");
        let timeout_secs = setting_int(&settings, "request_timeout_secs")
            .map(|n| n.max(1) as u64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);
        let extra_headers = setting_headers(&settings, "additional_headers");

        let model = resolve_model(&config.model).unwrap_or_else(|| {
            default_model
                .clone()
                .unwrap_or_else(|| "llama3.1".to_string())
        });

        let endpoint = build_endpoint(&base_url, "/api/chat")?;

        {
            // Ollama's text chat endpoint doesn't take Anthropic-style
            // image content blocks, so we pass only the text body here.
            // Attachments (if any) are silently dropped — the user can
            // still see them in the event log via the dispatch route.
            let mut conv = self.conversations.lock().await;
            let history = conv.entry(session_id.clone()).or_default();
            history.push(ChatMessage {
                role: "user".into(),
                content: message.text.clone(),
            });
            // O(turns²) bandwidth otherwise — Ollama is stateless so
            // we'd resend the entire transcript on every turn forever.
            trim_history(history, MAX_HISTORY_MESSAGES);
        }

        let cancel = Arc::new(Notify::new());
        let cancel_for_task = cancel.clone();
        let runs = self.runs.clone();
        let conversations = self.conversations.clone();
        let client = self.client.clone();
        let sid = session_id.clone();
        let model_label = config.model.clone();

        let handle = tokio::spawn(async move {
            let completed = run_chat_stream(
                &client,
                &endpoint,
                &model,
                &model_label,
                &sid,
                &db,
                &broadcaster,
                &conversations,
                extra_headers,
                timeout_secs,
                cancel_for_task,
            )
            .await;

            {
                let mut map = runs.lock().await;
                map.remove(&sid);
            }

            let _ = completion_tx
                .send(ProcessCompletion {
                    session_id: sid,
                    completed,
                })
                .await;
        });

        let mut runs_map = self.runs.lock().await;
        runs_map.insert(session_id, OllamaRun { handle, cancel });
        Ok(())
    }

    async fn cancel(&self, session_id: &str) {
        let cancel = {
            let runs = self.runs.lock().await;
            runs.get(session_id).map(|r| r.cancel.clone())
        };
        if let Some(c) = cancel {
            tracing::info!(session_id = %session_id, "Cancelling ollama run");
            c.notify_one();
        }
        // Per `wait_for_termination` semantics: drop history alongside
        // cancel so a fresh /clear-style restart begins from scratch.
        // Keys we still have a run for will be re-inserted by the next
        // send_message before any stream task reads them.
        self.conversations.lock().await.remove(session_id);
    }

    async fn interrupt(&self, session_id: &str) {
        self.cancel(session_id).await;
    }

    async fn write_stdin(&self, _session_id: &str, _text: &str) -> bool {
        // Ollama has no stdin channel: every interactive answer comes
        // through send_message as a fresh user turn.
        false
    }

    async fn is_running(&self, session_id: &str) -> bool {
        let runs = self.runs.lock().await;
        runs.get(session_id)
            .map(|r| !r.handle.is_finished())
            .unwrap_or(false)
    }

    async fn wait_for_termination(&self, session_id: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if !self.runs.lock().await.contains_key(session_id) {
                return;
            }
            if std::time::Instant::now() >= deadline {
                tracing::warn!(
                    session_id = %session_id,
                    "wait_for_termination timed out for ollama run"
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn cleanup(&self) {
        let mut runs = self.runs.lock().await;
        runs.retain(|_, r| !r.handle.is_finished());
    }

    async fn shutdown(&self) {
        let mut runs = self.runs.lock().await;
        for (_, run) in runs.drain() {
            run.cancel.notify_one();
            run.handle.abort();
        }
        self.conversations.lock().await.clear();
    }
}

/// Strip the `ollama:` provider prefix from a fully-qualified model id.
/// Returns `None` for bare model strings (the caller falls back to the
/// configured default model) or for prefixes that aren't ours.
fn resolve_model(raw: &str) -> Option<String> {
    if let Some(rest) = raw.strip_prefix("ollama:") {
        if rest.is_empty() {
            return None;
        }
        return Some(rest.to_string());
    }
    None
}

fn setting_str(settings: &HashMap<String, serde_json::Value>, key: &str) -> Option<String> {
    settings
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn setting_int(settings: &HashMap<String, serde_json::Value>, key: &str) -> Option<i64> {
    settings.get(key).and_then(|v| v.as_i64())
}

fn setting_bool(settings: &HashMap<String, serde_json::Value>, key: &str) -> Option<bool> {
    settings.get(key).and_then(|v| v.as_bool())
}

/// Parse Ollama's OpenAI-compatible `/v1/models` body into a list of
/// model ids. `None` on a malformed body so the caller falls back to the
/// static seed; `Some(vec![])` is a valid "server has no models pulled".
/// Blank ids are dropped.
fn parse_openai_models(body: &str) -> Option<Vec<String>> {
    let parsed: OpenAiModelsResponse = match serde_json::from_str(body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("ollama: failed to parse /v1/models response: {e}");
            return None;
        }
    };
    Some(
        parsed
            .data
            .into_iter()
            .map(|m| m.id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect(),
    )
}

/// Build a `ModelInfo` for a bare Ollama model name. The display name is
/// derived from the id so a discovered/registered model only needs its
/// name; every Ollama model advertises the `code` capability.
fn model_info(name: String) -> ModelInfo {
    ModelInfo {
        display_name: format!("{name} (Ollama)"),
        id: name,
        capabilities: vec!["code".into()],
    }
}

/// Extract a key-value list setting (normalized array of `{key,value}`)
/// as a flat `Vec<(name, value)>`. Returns an empty Vec when unset so
/// the caller never has to `.unwrap_or_default()`.
fn setting_headers(
    settings: &HashMap<String, serde_json::Value>,
    key: &str,
) -> Vec<(String, String)> {
    let Some(arr) = settings.get(key).and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|entry| {
            let k = entry.get("key").and_then(|v| v.as_str())?;
            let v = entry.get("value").and_then(|v| v.as_str())?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

/// Extract a `StringList` setting (a JSON array of strings) as a flat
/// `Vec<String>`, trimming entries and dropping blanks. Returns an empty
/// Vec when unset.
fn setting_str_list(settings: &HashMap<String, serde_json::Value>, key: &str) -> Vec<String> {
    let Some(arr) = settings.get(key).and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Merge the user's `additional_models` onto a base catalog (either the
/// autodiscovered server list or the static seed), as `ModelInfo`s. Skips
/// any extra whose id already appears in `base` (or earlier in the list)
/// so a duplicate of a discovered/built-in model doesn't show twice. The
/// display name is derived from the bare name so the user only has to
/// type the id.
fn merge_additional_models(base: Vec<ModelInfo>, extras: Vec<String>) -> Vec<ModelInfo> {
    let mut seen: std::collections::HashSet<String> = base.iter().map(|m| m.id.clone()).collect();
    let mut models = base;
    for name in extras {
        if seen.insert(name.clone()) {
            models.push(model_info(name));
        }
    }
    models
}

/// Join `base_url` with `path` so we don't end up with a `//api/chat`
/// when the user pasted a trailing slash.
fn build_endpoint(base_url: &str, path: &str) -> anyhow::Result<String> {
    let trimmed = base_url.trim_end_matches('/');
    let endpoint = format!("{trimmed}{path}");
    // Light re-validation: settings already enforced http(s)://, but the
    // string is reaching the HTTP client now, so reject anything that
    // would let an attacker repoint past the scheme check.
    let lower = endpoint.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(anyhow::anyhow!("ollama base_url must be http or https"));
    }
    Ok(endpoint)
}

#[allow(clippy::too_many_arguments)]
async fn run_chat_stream(
    client: &reqwest::Client,
    endpoint: &str,
    model: &str,
    model_label: &str,
    session_id: &str,
    db: &crate::db::Db,
    broadcaster: &crate::ws::broadcaster::Broadcaster,
    conversations: &Arc<Mutex<HashMap<String, Vec<ChatMessage>>>>,
    extra_headers: Vec<(String, String)>,
    timeout_secs: u64,
    cancel: Arc<Notify>,
) -> bool {
    emit_event(
        db,
        broadcaster,
        session_id,
        ProviderEvent::Started {
            model: model_label.to_string(),
            conversation_id: None,
            metadata: serde_json::json!({ "provider": "ollama" }),
        },
    )
    .await;

    let messages = {
        let map = conversations.lock().await;
        map.get(session_id).cloned().unwrap_or_default()
    };
    if messages.is_empty() {
        // Shouldn't happen — send_message inserts the user turn before
        // spawning this task — but emit a useful event instead of an
        // empty POST to Ollama.
        crash(db, broadcaster, session_id, "no messages to send", None).await;
        return false;
    }

    let body = ChatRequest {
        model,
        messages: &messages,
        stream: true,
    };

    let mut request = client
        .post(endpoint)
        .timeout(Duration::from_secs(timeout_secs))
        .header("Content-Type", "application/json")
        .json(&body);
    for (name, value) in &extra_headers {
        // reqwest fails the build if the header name/value are
        // malformed — settings validation enforces RFC-safe names, but
        // we still ignore malformed ones rather than poison the run.
        match (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            (Ok(n), Ok(v)) => request = request.header(n, v),
            _ => tracing::warn!(header = %name, "Dropping malformed Ollama header"),
        }
    }

    let response_fut = request.send();
    let response = tokio::select! {
        _ = cancel.notified() => {
            crash(db, broadcaster, session_id, "cancelled", None).await;
            return false;
        }
        r = response_fut => match r {
            Ok(r) => r,
            Err(e) => {
                crash(db, broadcaster, session_id, &format!("HTTP error: {e}"), None).await;
                return false;
            }
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        // Bound the error body so a giant HTML error page doesn't blow
        // up the event log.
        let body = response.text().await.unwrap_or_default();
        let truncated: String = body.chars().take(2_000).collect();
        crash(
            db,
            broadcaster,
            session_id,
            &format!("Ollama returned HTTP {status}"),
            Some(truncated),
        )
        .await;
        return false;
    }

    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut assistant_text = String::new();
    use futures_util::StreamExt;

    loop {
        tokio::select! {
            _ = cancel.notified() => {
                crash(db, broadcaster, session_id, "interrupted", None).await;
                return false;
            }
            chunk = stream.next() => {
                let Some(chunk) = chunk else { break; };
                let chunk = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        crash(
                            db,
                            broadcaster,
                            session_id,
                            &format!("stream error: {e}"),
                            None,
                        )
                        .await;
                        return false;
                    }
                };
                buffer.extend_from_slice(&chunk);
                // Ollama emits newline-delimited JSON; consume each
                // complete line and leave any partial line in the buffer.
                while let Some(nl) = buffer.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=nl).collect();
                    let trimmed = std::str::from_utf8(&line)
                        .unwrap_or("")
                        .trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let parsed: ChatStreamChunk = match serde_json::from_str(trimmed) {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(
                                session_id = %session_id,
                                "Skipping unparseable Ollama chunk: {e}"
                            );
                            continue;
                        }
                    };
                    if let Some(err) = parsed.error {
                        crash(db, broadcaster, session_id, &err, None).await;
                        return false;
                    }
                    if let Some(message) = parsed.message
                        && let Some(content) = message.content
                        && !content.is_empty()
                    {
                        assistant_text.push_str(&content);
                        emit_event(
                            db,
                            broadcaster,
                            session_id,
                            ProviderEvent::Text { text: content },
                        )
                        .await;
                    }
                    if parsed.done {
                        finalize(
                            db,
                            broadcaster,
                            session_id,
                            conversations,
                            assistant_text,
                        )
                        .await;
                        return true;
                    }
                }
            }
        }
    }

    // Stream closed without an explicit `done: true`. Treat as success
    // if we already received text (Ollama sometimes shuts the connection
    // before the last marker arrives); otherwise surface as a crash.
    if assistant_text.is_empty() {
        crash(
            db,
            broadcaster,
            session_id,
            "Ollama closed the stream before producing output",
            None,
        )
        .await;
        return false;
    }
    finalize(db, broadcaster, session_id, conversations, assistant_text).await;
    true
}

/// Convenience for emitting a `Crashed` event. Keeps the noisy
/// `exit_code: None, stderr: None` boilerplate out of every error site.
async fn crash(
    db: &crate::db::Db,
    broadcaster: &crate::ws::broadcaster::Broadcaster,
    session_id: &str,
    reason: &str,
    stderr: Option<String>,
) {
    emit_event(
        db,
        broadcaster,
        session_id,
        ProviderEvent::Crashed {
            reason: reason.to_string(),
            exit_code: None,
            stderr,
        },
    )
    .await;
}

async fn finalize(
    db: &crate::db::Db,
    broadcaster: &crate::ws::broadcaster::Broadcaster,
    session_id: &str,
    conversations: &Arc<Mutex<HashMap<String, Vec<ChatMessage>>>>,
    assistant_text: String,
) {
    {
        let mut map = conversations.lock().await;
        let history = map.entry(session_id.to_string()).or_default();
        history.push(ChatMessage {
            role: "assistant".into(),
            content: assistant_text,
        });
        trim_history(history, MAX_HISTORY_MESSAGES);
    }
    emit_event(
        db,
        broadcaster,
        session_id,
        ProviderEvent::Completed {
            conversation_id: None,
        },
    )
    .await;
}

/// Drop earlier messages once the transcript exceeds `cap`. Keeps the
/// most recent `cap` entries; never truncates mid-pair so the oldest
/// remaining message is always a `user`-role turn.
fn trim_history(history: &mut Vec<ChatMessage>, cap: usize) {
    if history.len() <= cap {
        return;
    }
    let drop_n = history.len() - cap;
    history.drain(0..drop_n);
    // Align to a user-message boundary: if we ended up with an
    // assistant turn first, drop one more so the next replay starts
    // from a user turn (Ollama tolerates either but the canonical
    // shape is user-first).
    if history.first().map(|m| m.role.as_str()) == Some("assistant") {
        history.remove(0);
    }
}

/// Static model list shown in the UI as the fallback catalog: the seed
/// registered at init, and what the picker shows when autodiscovery is
/// turned off or the server can't be reached. Users can still type any
/// model name they have pulled locally via `additional_models`.
pub fn default_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "llama3.1".into(),
            display_name: "Llama 3.1 (Ollama)".into(),
            capabilities: vec!["code".into()],
        },
        ModelInfo {
            id: "llama3.2".into(),
            display_name: "Llama 3.2 (Ollama)".into(),
            capabilities: vec!["code".into()],
        },
        ModelInfo {
            id: "qwen2.5-coder".into(),
            display_name: "Qwen 2.5 Coder (Ollama)".into(),
            capabilities: vec!["code".into()],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_model_strips_only_ollama_prefix() {
        assert_eq!(resolve_model("ollama:llama3.1"), Some("llama3.1".into()));
        assert_eq!(resolve_model("ollama:"), None);
        assert_eq!(resolve_model("llama3.1"), None);
        assert_eq!(resolve_model("claude:opus"), None);
    }

    #[test]
    fn setting_str_list_trims_and_drops_blanks() {
        let mut settings = HashMap::new();
        settings.insert(
            "additional_models".into(),
            serde_json::json!(["  llama3.1:8b ", "", "   ", "mistral-small"]),
        );
        assert_eq!(
            setting_str_list(&settings, "additional_models"),
            vec!["llama3.1:8b".to_string(), "mistral-small".to_string()]
        );
        // Unset key → empty, never a panic.
        assert!(setting_str_list(&settings, "missing").is_empty());
    }

    #[test]
    fn merge_additional_models_appends_and_dedups() {
        let merged = merge_additional_models(
            default_models(),
            vec![
                "llama3.1".into(),    // already a built-in id → skipped
                "llama3.1:8b".into(), // tag variant → kept
                "mistral-small".into(),
                "mistral-small".into(), // duplicate within extras → skipped
            ],
        );
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        // The three seeds survive, plus the two distinct extras, in order.
        assert_eq!(
            ids,
            vec![
                "llama3.1",
                "llama3.2",
                "qwen2.5-coder",
                "llama3.1:8b",
                "mistral-small",
            ]
        );
        // Extras carry a derived display name and the code capability.
        let extra = merged.iter().find(|m| m.id == "llama3.1:8b").unwrap();
        assert_eq!(extra.display_name, "llama3.1:8b (Ollama)");
        assert_eq!(extra.capabilities, vec!["code".to_string()]);
    }

    #[test]
    fn parse_openai_models_extracts_ids_and_handles_garbage() {
        let body = r#"{
            "object": "list",
            "data": [
                {"id": "llama3.1:8b", "object": "model", "created": 1, "owned_by": "library"},
                {"id": "  ", "object": "model"},
                {"id": "qwen2.5-coder:7b", "object": "model"}
            ]
        }"#;
        // Ids extracted in order; blank ids dropped; tag colons preserved.
        assert_eq!(
            parse_openai_models(body),
            Some(vec![
                "llama3.1:8b".to_string(),
                "qwen2.5-coder:7b".to_string()
            ])
        );
        // A valid-but-empty data array is a legitimate "nothing pulled".
        assert_eq!(parse_openai_models(r#"{"data":[]}"#), Some(Vec::new()));
        // Garbage → None so the caller falls back to the static seed.
        assert_eq!(parse_openai_models("not json at all"), None);
    }

    #[test]
    fn merge_additional_models_onto_discovered_base_dedups() {
        // Discovery returned two models; the user also registered one of
        // them plus a custom name. The duplicate is dropped, the custom
        // one appended, and the static seed is *not* involved.
        let base: Vec<ModelInfo> = ["llama3.1:8b", "qwen2.5-coder:7b"]
            .into_iter()
            .map(|s| model_info(s.to_string()))
            .collect();
        let merged =
            merge_additional_models(base, vec!["llama3.1:8b".into(), "me/custom-model".into()]);
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["llama3.1:8b", "qwen2.5-coder:7b", "me/custom-model"]
        );
    }

    #[test]
    fn build_endpoint_normalizes_trailing_slash() {
        assert_eq!(
            build_endpoint("http://localhost:11434", "/api/chat").unwrap(),
            "http://localhost:11434/api/chat"
        );
        assert_eq!(
            build_endpoint("http://localhost:11434/", "/api/chat").unwrap(),
            "http://localhost:11434/api/chat"
        );
    }

    #[test]
    fn build_endpoint_rejects_non_http_schemes() {
        // Defence-in-depth — the settings validator already enforces
        // http(s), but the provider re-checks before hitting the
        // network in case the settings layer evolves.
        assert!(build_endpoint("file:///etc/passwd", "/api/chat").is_err());
        assert!(build_endpoint("gopher://example.com", "/api/chat").is_err());
    }

    #[test]
    fn trim_history_caps_at_recent_messages_and_aligns_to_user_turn() {
        // Build 6 turns (12 messages) — well past a cap of 4.
        let mut history = Vec::new();
        for i in 0..6 {
            history.push(ChatMessage {
                role: "user".into(),
                content: format!("u{i}"),
            });
            history.push(ChatMessage {
                role: "assistant".into(),
                content: format!("a{i}"),
            });
        }
        trim_history(&mut history, 4);
        assert_eq!(history.len(), 4);
        assert_eq!(
            history[0].role, "user",
            "transcript must start on a user turn"
        );
        // The four most recent messages survive (u4, a4, u5, a5 — but
        // since trimming might align away one, just spot-check.)
        assert!(history.last().unwrap().content.starts_with('a'));
    }
}
