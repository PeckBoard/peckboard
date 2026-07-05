//! Ollama agent provider.
//!
//! Hits Ollama's HTTP `/api/chat` endpoint in streaming mode and
//! translates each `{"message":{"content":"…"}}` chunk into a
//! [`ProviderEvent::Text`].
//!
//! When the `enable_tools` setting is on (the default), every turn also
//! offers the model Peckboard's MCP tools — core tools plus any active
//! plugin tools, the same set the `/mcp` route serves — in the `tools`
//! field of the chat body. If the model answers with `tool_calls`, the
//! provider executes each via the shared
//! [`crate::service::mcp_server::dispatch_tool_call`] (so plugin- and
//! core-owned tools resolve identically to the `/mcp` route), feeds the
//! results back as `role:"tool"` messages, and re-prompts — looping until
//! the model stops calling tools or [`MAX_TOOL_ROUNDS`] is hit. Tool
//! activity surfaces as `ToolStart`/`ToolEnd` events. With tools off (or
//! none available) the stream is the minimal `Started → Text* →
//! Completed | Crashed`.
//!
//! Ollama itself exposes no conversation IDs or permission prompts, so
//! those parts of the event stream stay unused.
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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::plugin::manager::PluginManager;
use crate::plugin::settings::PluginSettingsStore;
use crate::provider::agent::{AgentProvider, ProcessCompletion, SendMessageContext, emit_event};
use crate::provider::stream::{ModelInfo, ProviderEvent};
use crate::service::mcp_server::{McpToolRegistry, ToolCallContext, dispatch_tool_call};

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

/// Hard cap on tool-call rounds within a single turn. Each round is one
/// model response that asked for tools plus the tool results fed back. A
/// model that keeps calling tools without ever producing a final answer
/// (or two that ping-pong) would otherwise loop until the request
/// timeout; this bounds it and surfaces a clear error instead.
const MAX_TOOL_ROUNDS: usize = 16;

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
    /// Base64-encoded images attached to a `user` turn, in Ollama's
    /// `/api/chat` shape (a bare array of base64 strings, no data-URI
    /// prefix). Omitted entirely when the turn carries no images, so a
    /// text-only request is byte-for-byte what it was before image
    /// support existed. Only multimodal models actually look at this;
    /// text models ignore it.
    #[serde(skip_serializing_if = "Option::is_none")]
    images: Option<Vec<String>>,
    /// Tool calls on an `assistant` turn, replayed verbatim on the next
    /// request so the model sees its own call alongside the `tool` result.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCall>>,
    /// Names the tool a `role:"tool"` result belongs to. Ollama matches the
    /// result to the pending call by this name.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
}

impl ChatMessage {
    fn system(content: String) -> Self {
        ChatMessage {
            role: "system".into(),
            content,
            images: None,
            tool_calls: None,
            tool_name: None,
        }
    }

    fn user(content: String, images: Option<Vec<String>>) -> Self {
        ChatMessage {
            role: "user".into(),
            content,
            images,
            tool_calls: None,
            tool_name: None,
        }
    }

    fn assistant(content: String, tool_calls: Option<Vec<ToolCall>>) -> Self {
        ChatMessage {
            role: "assistant".into(),
            content,
            images: None,
            tool_calls,
            tool_name: None,
        }
    }

    fn tool_result(tool_name: String, content: String) -> Self {
        ChatMessage {
            role: "tool".into(),
            content,
            images: None,
            tool_calls: None,
            tool_name: Some(tool_name),
        }
    }
}

/// One tool call, matching Ollama's `/api/chat` shape in both directions:
/// the assistant emits these in `message.tool_calls`, and we replay them
/// unchanged on the assistant turn of the next request.
#[derive(Clone, Serialize, Deserialize)]
struct ToolCall {
    function: ToolCallFunction,
}

#[derive(Clone, Serialize, Deserialize)]
struct ToolCallFunction {
    name: String,
    /// Ollama delivers arguments as a JSON object (not a string), and
    /// accepts the same shape on replay.
    #[serde(default)]
    arguments: serde_json::Value,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
    /// Tool definitions in Ollama's `{type:"function", function:{…}}` shape.
    /// Omitted entirely when tools are disabled or none are available, so a
    /// plain chat request is byte-for-byte what it was before tools existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [serde_json::Value]>,
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
    /// Tool calls the model wants run this turn. Ollama emits each as a
    /// complete object within a streamed chunk (not split across deltas),
    /// so we can collect them as they arrive and dispatch once `done`.
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
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

/// Subset of Ollama's `POST /api/show` response. We only need the
/// `capabilities` array (e.g. `["completion","tools","vision","thinking"]`)
/// to tell whether a model can take tool calls and/or images; the rest of
/// the payload (modelfile, template, license, details) is ignored.
#[derive(Deserialize)]
struct ShowResponse {
    #[serde(default)]
    capabilities: Vec<String>,
}

/// Cached outcome of a per-model `/api/show` capability probe. `None`
/// means the last probe failed (cached either way, like [`DiscoveryCache`],
/// so a wedged/old server isn't re-probed on every turn). See
/// [`MODEL_DISCOVERY_TTL`].
struct CapabilityCacheEntry {
    fetched_at: Instant,
    capabilities: Option<Vec<String>>,
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
    /// TTL cache of per-model `/api/show` capability probes, keyed by the
    /// bare model id. Lets both the model picker and the per-turn request
    /// path know whether a model supports tools/vision without re-probing
    /// the server every time.
    capability_cache: Arc<Mutex<HashMap<String, CapabilityCacheEntry>>>,
    client: reqwest::Client,
}

impl OllamaProvider {
    pub fn new(settings: PluginSettingsStore) -> Self {
        OllamaProvider {
            settings,
            runs: Arc::new(Mutex::new(HashMap::new())),
            conversations: Arc::new(Mutex::new(HashMap::new())),
            discovery_cache: Arc::new(Mutex::new(None)),
            capability_cache: Arc::new(Mutex::new(HashMap::new())),
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

    /// Capabilities Ollama reports for `model` via `/api/show`, e.g.
    /// `["completion","tools","vision"]`. Goes through a per-model TTL
    /// cache so neither the picker nor the chat path probes on every use.
    /// `None` when the last probe failed; callers treat that as "unknown"
    /// and fall back to permissive defaults so an unreachable `/api/show`
    /// never breaks an otherwise-working setup.
    async fn model_capabilities(
        &self,
        settings: &HashMap<String, serde_json::Value>,
        model: &str,
    ) -> Option<Vec<String>> {
        {
            let cache = self.capability_cache.lock().await;
            if let Some(entry) = cache.get(model)
                && entry.fetched_at.elapsed() < MODEL_DISCOVERY_TTL
            {
                return entry.capabilities.clone();
            }
        }
        let result = self.fetch_model_capabilities(settings, model).await;
        let mut cache = self.capability_cache.lock().await;
        cache.insert(
            model.to_string(),
            CapabilityCacheEntry {
                fetched_at: Instant::now(),
                capabilities: result.clone(),
            },
        );
        result
    }

    /// Hit `<base_url>/api/show` for one model and return its reported
    /// `capabilities`. Returns `None` (so the caller assumes nothing) on
    /// any failure: no `base_url`, a bad URL, a network/HTTP error, or an
    /// unparseable body.
    async fn fetch_model_capabilities(
        &self,
        settings: &HashMap<String, serde_json::Value>,
        model: &str,
    ) -> Option<Vec<String>> {
        let base_url = setting_str(settings, "base_url")?;
        let endpoint = match build_endpoint(&base_url, "/api/show") {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("ollama: bad base_url for capability probe: {e}");
                return None;
            }
        };

        let mut request = self
            .client
            .post(&endpoint)
            .timeout(Duration::from_secs(MODEL_DISCOVERY_TIMEOUT_SECS))
            .json(&serde_json::json!({ "model": model }));
        // Same auth-proxy headers the chat and discovery paths use.
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
                tracing::warn!(model = %model, "ollama: capability probe request failed: {e}");
                return None;
            }
        };
        if !response.status().is_success() {
            tracing::warn!(
                model = %model,
                "ollama: /api/show returned HTTP {}",
                response.status()
            );
            return None;
        }
        let body = match response.text().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(model = %model, "ollama: failed to read /api/show body: {e}");
                return None;
            }
        };
        match serde_json::from_str::<ShowResponse>(&body) {
            Ok(show) => Some(show.capabilities),
            Err(e) => {
                tracing::warn!(model = %model, "ollama: failed to parse /api/show response: {e}");
                None
            }
        }
    }
}

#[async_trait]
impl AgentProvider for OllamaProvider {
    fn id(&self) -> &str {
        "ollama"
    }

    fn model_price(&self, _model_id: &str) -> Option<(f64, f64)> {
        // Local inference — no per-token billing.
        Some((0.0, 0.0))
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
                // Probe each discovered model's `/api/show` capabilities so
                // the picker can show `tools`/`vision` tags (cached, so this
                // only costs a round-trip per model once per TTL window).
                Some(names) => {
                    let mut infos = Vec::with_capacity(names.len());
                    for name in names {
                        let caps = self.model_capabilities(&settings, &name).await;
                        infos.push(model_info_with_caps(name, caps));
                    }
                    infos
                }
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
        let SendMessageContext {
            session_id,
            message,
            db,
            broadcaster,
            config,
            conversation_id: _,
            completion_tx,
            // The plugin host backs MCP tool calls (and the tool-observer
            // hooks); see `run_chat_stream`'s tool loop.
            plugins,
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
        let enable_tools = setting_bool(&settings, "enable_tools").unwrap_or(true);

        let model = resolve_model(&config.model).unwrap_or_else(|| {
            default_model
                .clone()
                .unwrap_or_else(|| "llama3.1".to_string())
        });

        // Probe what this model actually supports before building the
        // request. Ollama 400s a `/api/chat` that carries `tools` for a
        // model whose template has no tool support (e.g. gemma-based
        // models), and a non-vision model has nowhere to put images. When
        // the probe itself fails we fall back to permissive defaults so an
        // unreachable `/api/show` never breaks an otherwise-working setup.
        let capabilities = self.model_capabilities(&settings, &model).await;
        let supports_tools = capability_present(capabilities.as_deref(), "tools");
        let supports_vision = capability_present(capabilities.as_deref(), "vision");
        if enable_tools && !supports_tools {
            tracing::info!(
                model = %model,
                "ollama: model does not advertise tool support; sending a tool-free request"
            );
        }
        let enable_tools = enable_tools && supports_tools;

        let endpoint = build_endpoint(&base_url, "/api/chat")?;

        {
            // Ollama's `/api/chat` takes images as a per-message array of
            // bare base64 strings (no data-URI prefix). Build that array
            // from any image attachments on this turn; non-image files are
            // dropped with a warning since the chat endpoint has nowhere to
            // put them. Multimodal models (llava, llama3.2-vision, …) read
            // these; text-only models ignore the field.
            let images = encode_image_attachments(&message.attachments);
            // Only forward images to a model that advertises vision; a
            // text-only model would reject or silently mangle them.
            let images = if supports_vision {
                images
            } else {
                if images.is_some() {
                    tracing::warn!(
                        model = %model,
                        "ollama: model does not advertise vision support; dropping image attachment(s)"
                    );
                }
                None
            };
            let mut conv = self.conversations.lock().await;
            let history = conv.entry(session_id.clone()).or_default();
            history.push(ChatMessage::user(message.text.clone(), images));
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
        // The per-session override fully replaces the system prompt; with no
        // override, every session ships the shared working-style rules.
        let system_prompt = config
            .system_prompt_override
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(crate::provider::WORKING_STYLE)
            .to_string();

        let handle = tokio::spawn(async move {
            let completed = run_chat_stream(ChatStreamArgs {
                client: &client,
                endpoint: &endpoint,
                model: &model,
                model_label: &model_label,
                session_id: &sid,
                db: &db,
                broadcaster: &broadcaster,
                conversations: &conversations,
                plugins: &plugins,
                enable_tools,
                extra_headers,
                timeout_secs,
                cancel: cancel_for_task,
                system_prompt,
            })
            .await;

            {
                let mut map = runs.lock().await;
                map.remove(&sid);
            }

            let _ = completion_tx
                .send(ProcessCompletion {
                    session_id: sid,
                    completed,
                    error: None,
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

/// Base64-encode the image attachments on a user turn into the bare-string
/// array Ollama's `/api/chat` expects. Non-image attachments are dropped
/// with a warning — the chat endpoint only accepts images. Returns `None`
/// (rather than an empty vec) when there are no images, so the `images`
/// field is omitted from the request entirely and a text-only turn
/// serializes exactly as it did before image support.
fn encode_image_attachments(
    attachments: &[crate::provider::message::UserAttachment],
) -> Option<Vec<String>> {
    use base64::Engine as _;

    let mut images = Vec::new();
    for att in attachments {
        if att.mime_type.starts_with("image/") {
            images.push(base64::engine::general_purpose::STANDARD.encode(&att.data));
        } else {
            tracing::warn!(
                filename = %att.filename,
                mime = %att.mime_type,
                "Dropping non-image attachment — Ollama /api/chat only accepts images"
            );
        }
    }

    if images.is_empty() {
        None
    } else {
        Some(images)
    }
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

/// Whether `capabilities` (as reported by `/api/show`) contains `cap`.
/// `None` means the probe failed / capabilities are unknown — treated as
/// permissive (`true`) so an unreachable `/api/show` never strips tools or
/// images from a model that would otherwise have worked.
fn capability_present(capabilities: Option<&[String]>, cap: &str) -> bool {
    match capabilities {
        Some(caps) => caps.iter().any(|c| c == cap),
        None => true,
    }
}

/// Build a `ModelInfo`, folding in any capabilities Ollama reported for the
/// model via `/api/show`. Every Ollama model keeps the baseline `code` tag;
/// `tools` and `vision` are added only when the server says the model
/// supports them, so the picker reflects what the model can actually do. A
/// failed probe (`None`) falls back to the baseline tag alone.
fn model_info_with_caps(name: String, caps: Option<Vec<String>>) -> ModelInfo {
    let mut capabilities = vec!["code".into()];
    if let Some(caps) = caps {
        if caps.iter().any(|c| c == "tools") {
            capabilities.push("tools".into());
        }
        if caps.iter().any(|c| c == "vision") {
            capabilities.push("vision".into());
        }
    }
    ModelInfo {
        display_name: format!("{name} (Ollama)"),
        id: name,
        capabilities,
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

/// Everything one `run_chat_stream` task needs. Bundled into a struct so the
/// per-turn entry point stays a single argument instead of a dozen
/// positional ones (and so adding a knob later doesn't reshuffle call sites).
struct ChatStreamArgs<'a> {
    client: &'a reqwest::Client,
    endpoint: &'a str,
    model: &'a str,
    model_label: &'a str,
    session_id: &'a str,
    db: &'a crate::db::Db,
    broadcaster: &'a Arc<crate::ws::broadcaster::Broadcaster>,
    conversations: &'a Arc<Mutex<HashMap<String, Vec<ChatMessage>>>>,
    plugins: &'a PluginManager,
    enable_tools: bool,
    extra_headers: Vec<(String, String)>,
    timeout_secs: u64,
    cancel: Arc<Notify>,
    /// System prompt for this turn: the per-session override if set,
    /// otherwise the shared working-style rules. Prepended as the first
    /// `role:"system"` message of the outgoing request (not persisted to
    /// the transcript, so history stays user-first).
    system_prompt: String,
}

/// Outcome of streaming one model response (one `/api/chat` call).
enum RoundOutcome {
    /// The response finished cleanly: any text was streamed out as it
    /// arrived, and `tool_calls` holds whatever tools the model asked for
    /// (empty when it just answered).
    Message {
        text: String,
        tool_calls: Vec<ToolCall>,
    },
    /// A failure was already reported via a `Crashed` event; the caller
    /// should stop and report the turn as not-completed.
    Failed,
}

/// Drive one turn: offer tools (when enabled), stream the model's reply, run
/// any tool calls it makes, and loop until it answers without calling a tool
/// or [`MAX_TOOL_ROUNDS`] is reached. Returns `true` on a clean finish.
async fn run_chat_stream(args: ChatStreamArgs<'_>) -> bool {
    let ChatStreamArgs {
        client,
        endpoint,
        model,
        model_label,
        session_id,
        db,
        broadcaster: broadcaster_arc,
        conversations,
        plugins,
        enable_tools,
        extra_headers,
        timeout_secs,
        cancel,
        system_prompt,
    } = args;
    let broadcaster: &crate::ws::broadcaster::Broadcaster = broadcaster_arc.as_ref();

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

    let mut messages = {
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

    // Prepend the system prompt to THIS request's messages only. It's not
    // pushed to `new_messages` (the persisted transcript), so history stays
    // user-first; it rides along on every outgoing request via this local
    // copy, which `messages.push(...)` extends as the turn progresses.
    if !system_prompt.trim().is_empty() {
        messages.insert(0, ChatMessage::system(system_prompt.clone()));
    }

    // Build the tool offer once for the turn. Running a tool needs a scoped
    // `ToolCallContext` (folder/project boundary); if we can't resolve one we
    // fall back to a plain, tool-free chat rather than failing the turn.
    let registry = McpToolRegistry::new();
    let (tools, tool_ctx) = if enable_tools {
        let defs = collect_ollama_tools(&registry, plugins).await;
        match build_tool_context(db, broadcaster_arc, session_id).await {
            Some(ctx) if !defs.is_empty() => (Some(defs), Some(ctx)),
            _ => (None, None),
        }
    } else {
        (None, None)
    };

    // Assistant + tool turns produced this turn, appended to the persistent
    // transcript once we finish so the next turn replays the full exchange.
    let mut new_messages: Vec<ChatMessage> = Vec::new();

    for _round in 0..MAX_TOOL_ROUNDS {
        let outcome = stream_one_round(StreamRound {
            client,
            endpoint,
            model,
            messages: &messages,
            tools: tools.as_deref(),
            extra_headers: &extra_headers,
            timeout_secs,
            db,
            broadcaster,
            session_id,
            cancel: &cancel,
        })
        .await;
        let (text, tool_calls) = match outcome {
            RoundOutcome::Message { text, tool_calls } => (text, tool_calls),
            RoundOutcome::Failed => return false,
        };

        // Record the assistant turn (its text plus the calls it requested)
        // so it replays alongside the tool results on the next round.
        let calls_for_replay = (!tool_calls.is_empty()).then(|| tool_calls.clone());
        let assistant = ChatMessage::assistant(text, calls_for_replay);
        messages.push(assistant.clone());
        new_messages.push(assistant);

        // No tools requested (or none we can run): this turn is the answer.
        let ctx = match tool_ctx.as_ref() {
            Some(ctx) if !tool_calls.is_empty() => ctx,
            _ => {
                finalize(db, broadcaster, session_id, conversations, new_messages).await;
                return true;
            }
        };

        // Run each requested tool, feeding the result back as a `tool` turn.
        for call in tool_calls {
            let tool_msg = tokio::select! {
                _ = cancel.notified() => {
                    crash(db, broadcaster, session_id, "interrupted", None).await;
                    return false;
                }
                m = run_one_tool(plugins, &registry, ctx, db, broadcaster, session_id, &call) => m,
            };
            messages.push(tool_msg.clone());
            new_messages.push(tool_msg);
        }
    }

    // Ran out of rounds without the model settling on a final answer. Persist
    // what we have (so the partial exchange isn't lost) and report the cap.
    {
        let mut map = conversations.lock().await;
        let history = map.entry(session_id.to_string()).or_default();
        history.extend(new_messages);
        trim_history(history, MAX_HISTORY_MESSAGES);
    }
    crash(
        db,
        broadcaster,
        session_id,
        &format!("stopped after {MAX_TOOL_ROUNDS} tool rounds without a final answer"),
        None,
    )
    .await;
    false
}

/// Inputs to [`stream_one_round`]. Same rationale as [`ChatStreamArgs`] —
/// keeps the streaming step a single positional argument.
struct StreamRound<'a> {
    client: &'a reqwest::Client,
    endpoint: &'a str,
    model: &'a str,
    messages: &'a [ChatMessage],
    tools: Option<&'a [serde_json::Value]>,
    extra_headers: &'a [(String, String)],
    timeout_secs: u64,
    db: &'a crate::db::Db,
    broadcaster: &'a crate::ws::broadcaster::Broadcaster,
    session_id: &'a str,
    cancel: &'a Notify,
}

/// POST one `/api/chat` request and consume its streamed reply: stream text
/// out as `Text` events as it arrives, and collect any `tool_calls`. Ollama
/// emits each tool call as a whole object inside a chunk (not split across
/// deltas), so they accumulate cleanly. Reports failures via `Crashed` and
/// returns [`RoundOutcome::Failed`].
async fn stream_one_round(round: StreamRound<'_>) -> RoundOutcome {
    let StreamRound {
        client,
        endpoint,
        model,
        messages,
        tools,
        extra_headers,
        timeout_secs,
        db,
        broadcaster,
        session_id,
        cancel,
    } = round;

    let body = ChatRequest {
        model,
        messages,
        stream: true,
        tools,
    };

    let mut request = client
        .post(endpoint)
        .timeout(Duration::from_secs(timeout_secs))
        .header("Content-Type", "application/json")
        .json(&body);
    for (name, value) in extra_headers {
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
            // Use the same reason ("interrupted") as the streaming/tool-call
            // cancel branches: the UI only suppresses the paired "Agent
            // crashed" banner when the agent-end reason is exactly
            // "interrupted", so a cancel during the initial request must
            // match or it surfaces as a spurious crash.
            crash(db, broadcaster, session_id, "interrupted", None).await;
            return RoundOutcome::Failed;
        }
        r = response_fut => match r {
            Ok(r) => r,
            Err(e) => {
                crash(db, broadcaster, session_id, &format!("HTTP error: {e}"), None).await;
                return RoundOutcome::Failed;
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
        return RoundOutcome::Failed;
    }

    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut text = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut done = false;
    use futures_util::StreamExt;

    while !done {
        tokio::select! {
            _ = cancel.notified() => {
                crash(db, broadcaster, session_id, "interrupted", None).await;
                return RoundOutcome::Failed;
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
                        return RoundOutcome::Failed;
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
                        return RoundOutcome::Failed;
                    }
                    if let Some(message) = parsed.message {
                        if let Some(content) = message.content
                            && !content.is_empty()
                        {
                            text.push_str(&content);
                            emit_event(
                                db,
                                broadcaster,
                                session_id,
                                ProviderEvent::Text { text: content },
                            )
                            .await;
                        }
                        if let Some(calls) = message.tool_calls {
                            tool_calls.extend(calls);
                        }
                    }
                    if parsed.done {
                        done = true;
                    }
                }
            }
        }
    }

    // A stream that closed before `done` with nothing at all to show is the
    // one genuine failure here; text-only or tool-only closes are fine.
    if !done && text.is_empty() && tool_calls.is_empty() {
        crash(
            db,
            broadcaster,
            session_id,
            "Ollama closed the stream before producing output",
            None,
        )
        .await;
        return RoundOutcome::Failed;
    }

    RoundOutcome::Message { text, tool_calls }
}

/// Every MCP tool offered to the model this turn — core tools plus active
/// plugin tools — in Ollama's `{type:"function", function:{…}}` shape. A
/// plugin tool whose name collides with a core tool is dropped (core wins),
/// matching the `/mcp` route's `tools/list`.
async fn collect_ollama_tools(
    registry: &McpToolRegistry,
    plugins: &PluginManager,
) -> Vec<serde_json::Value> {
    let core = registry.tool_definitions();
    let core_names: HashSet<&str> = core.iter().map(|t| t.name.as_str()).collect();
    let mut tools: Vec<serde_json::Value> = core
        .iter()
        .map(|t| ollama_tool_def(&t.name, &t.description, &t.input_schema))
        .collect();
    for t in plugins.mcp_tools().await {
        if core_names.contains(t.name.as_str()) {
            tracing::warn!(
                plugin = %t.plugin, tool = %t.name,
                "plugin mcp_tool collides with a core tool name; dropping"
            );
            continue;
        }
        tools.push(ollama_tool_def(&t.name, &t.description, &t.input_schema));
    }
    tools
}

/// One tool entry in Ollama's chat-request `tools` array.
fn ollama_tool_def(
    name: &str,
    description: &str,
    parameters: &serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters,
        },
    })
}

/// Resolve the scoped [`ToolCallContext`] for `session_id` from its session
/// row (the folder boundary every scope check enforces). `None` — no session
/// row — means we can't safely run tools, so the caller offers none.
/// `provider_registry`/`data_dir` are `None`: the few core tools that need
/// them (e.g. `list_models`, report export) degrade; plugin tools and the
/// rest work unaffected.
async fn build_tool_context(
    db: &crate::db::Db,
    broadcaster: &Arc<crate::ws::broadcaster::Broadcaster>,
    session_id: &str,
) -> Option<ToolCallContext> {
    let session = match db.get_session(session_id).await {
        Ok(Some(s)) => s,
        _ => {
            tracing::warn!(
                session_id = %session_id,
                "ollama: no session row found; running tool-free for this turn"
            );
            return None;
        }
    };
    Some(ToolCallContext {
        session_id: session_id.to_string(),
        project_id: session.project_id.clone(),
        card_id: session.card_id.clone(),
        folder_id: session.folder_id.clone(),
        db: Arc::new(db.clone()),
        broadcaster: broadcaster.clone(),
        provider_registry: None,
        data_dir: None,
    })
}

/// Execute one tool call and turn it into a `role:"tool"` reply. Emits
/// `ToolStart`/`ToolEnd` around the dispatch. A tool error is fed back to the
/// model as `{"error": …}` (not a hard failure) so it can recover or retry.
async fn run_one_tool(
    plugins: &PluginManager,
    registry: &McpToolRegistry,
    ctx: &ToolCallContext,
    db: &crate::db::Db,
    broadcaster: &crate::ws::broadcaster::Broadcaster,
    session_id: &str,
    call: &ToolCall,
) -> ChatMessage {
    let name = call.function.name.clone();
    let args = call.function.arguments.clone();
    let tool_use_id = uuid::Uuid::new_v4().to_string();

    emit_event(
        db,
        broadcaster,
        session_id,
        ProviderEvent::ToolStart {
            tool_use_id: tool_use_id.clone(),
            name: name.clone(),
            input: args.clone(),
        },
    )
    .await;

    match dispatch_tool_call(plugins, registry, &name, args, ctx).await {
        Ok(value) => {
            let text = serde_json::to_string(&value).unwrap_or_default();
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::ToolEnd {
                    tool_use_id,
                    output: Some(text.clone()),
                    error: None,
                    images: Vec::new(),
                },
            )
            .await;
            ChatMessage::tool_result(name, text)
        }
        Err(e) => {
            let err = e.to_string();
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::ToolEnd {
                    tool_use_id,
                    output: None,
                    error: Some(err.clone()),
                    images: Vec::new(),
                },
            )
            .await;
            ChatMessage::tool_result(name, serde_json::json!({ "error": err }).to_string())
        }
    }
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

/// Append this turn's generated messages (the final assistant turn, plus any
/// assistant-tool-call / tool-result pairs from earlier rounds) to the
/// persistent transcript and emit `Completed`.
async fn finalize(
    db: &crate::db::Db,
    broadcaster: &crate::ws::broadcaster::Broadcaster,
    session_id: &str,
    conversations: &Arc<Mutex<HashMap<String, Vec<ChatMessage>>>>,
    new_messages: Vec<ChatMessage>,
) {
    {
        let mut map = conversations.lock().await;
        let history = map.entry(session_id.to_string()).or_default();
        history.extend(new_messages);
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
    // Align to a user-message boundary: drop any leading assistant/tool
    // turns so the next replay starts from a user turn. A leading `tool`
    // result (or an assistant turn whose `tool_calls` were trimmed off the
    // front) would be a dangling reference Ollama can reject; user-first is
    // the canonical, always-valid shape.
    while matches!(
        history.first().map(|m| m.role.as_str()),
        Some("assistant") | Some("tool")
    ) {
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
            history.push(ChatMessage::user(format!("u{i}"), None));
            history.push(ChatMessage::assistant(format!("a{i}"), None));
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

    #[test]
    fn trim_history_drops_leading_tool_and_assistant_turns() {
        // A trim that lands on a tool-call exchange must not leave a
        // dangling `assistant(tool_calls)` / `tool` result at the front —
        // Ollama can reject a transcript that doesn't start on a user turn.
        let mut history = vec![
            ChatMessage::assistant(
                String::new(),
                Some(vec![ToolCall {
                    function: ToolCallFunction {
                        name: "math".into(),
                        arguments: serde_json::json!({ "expression": "1+1" }),
                    },
                }]),
            ),
            ChatMessage::tool_result("math".into(), "2".into()),
            ChatMessage::user("u1".into(), None),
            ChatMessage::assistant("a1".into(), None),
        ];
        trim_history(&mut history, 3);
        assert_eq!(
            history.iter().map(|m| m.role.as_str()).collect::<Vec<_>>(),
            vec!["user", "assistant"],
            "leading assistant+tool turns are dropped to land on a user turn"
        );
    }

    #[test]
    fn ollama_tool_def_has_function_envelope() {
        let def = ollama_tool_def(
            "math",
            "Evaluate an expression",
            &serde_json::json!({ "type": "object" }),
        );
        assert_eq!(def["type"], "function");
        assert_eq!(def["function"]["name"], "math");
        assert_eq!(def["function"]["description"], "Evaluate an expression");
        assert_eq!(def["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn chat_request_omits_tools_when_none() {
        // A tool-free request must serialize exactly as before tools existed:
        // no `tools` key, and plain user/assistant messages carry no
        // `tool_calls`/`tool_name` noise.
        let messages = vec![ChatMessage::user("hi".into(), None)];
        let body = ChatRequest {
            model: "llama3.1",
            messages: &messages,
            stream: true,
            tools: None,
        };
        let v = serde_json::to_value(&body).unwrap();
        assert!(v.get("tools").is_none(), "no tools key when None");
        let msg = &v["messages"][0];
        assert_eq!(msg["role"], "user");
        assert!(msg.get("tool_calls").is_none());
        assert!(msg.get("tool_name").is_none());
        // No attachments → no `images` key, so a text turn is unchanged.
        assert!(msg.get("images").is_none());
    }

    #[test]
    fn system_message_leads_request_and_carries_the_rules() {
        // Mirrors how run_chat_stream builds the outgoing request: the system
        // prompt is prepended as the first message, ahead of the user turn.
        let mut messages = vec![ChatMessage::user("hi".into(), None)];
        messages.insert(
            0,
            ChatMessage::system(crate::provider::WORKING_STYLE.to_string()),
        );
        let body = ChatRequest {
            model: "llama3.1",
            messages: &messages,
            stream: true,
            tools: None,
        };
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["messages"][0]["role"], "system");
        assert!(
            v["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("# Working style")
        );
        // The user turn is still present, right after the system message.
        assert_eq!(v["messages"][1]["role"], "user");
    }

    #[test]
    fn encode_image_attachments_keeps_images_drops_other() {
        use crate::provider::message::UserAttachment;

        // No attachments → None, so the request omits `images` entirely.
        assert!(encode_image_attachments(&[]).is_none());

        // A non-image-only set still yields None (nothing the chat
        // endpoint can use), so the field stays omitted.
        let pdf = UserAttachment {
            filename: "doc.pdf".into(),
            mime_type: "application/pdf".into(),
            data: b"%PDF".to_vec(),
        };
        assert!(encode_image_attachments(std::slice::from_ref(&pdf)).is_none());

        // An image is base64-encoded; the non-image alongside it is dropped.
        let png = UserAttachment {
            filename: "shot.png".into(),
            mime_type: "image/png".into(),
            data: vec![1, 2, 3, 4],
        };
        let images = encode_image_attachments(&[png, pdf]).expect("one image survives");
        assert_eq!(images.len(), 1);
        use base64::Engine as _;
        assert_eq!(
            images[0],
            base64::engine::general_purpose::STANDARD.encode([1, 2, 3, 4])
        );
    }

    #[test]
    fn user_message_serializes_images_field() {
        let msg = ChatMessage::user("what is this?".into(), Some(vec!["aGk=".into()]));
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"], "what is this?");
        assert_eq!(v["images"][0], "aGk=");
    }

    #[test]
    fn tool_call_round_trips_through_ollama_shape() {
        // The assistant's streamed tool call parses, and replaying it (plus
        // the matching tool result) serializes back into Ollama's shape.
        let chunk = r#"{"message":{"content":"","tool_calls":[
            {"function":{"name":"math","arguments":{"expression":"2+2"}}}
        ]},"done":false}"#;
        let parsed: ChatStreamChunk = serde_json::from_str(chunk).unwrap();
        let calls = parsed.message.unwrap().tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "math");
        assert_eq!(calls[0].function.arguments["expression"], "2+2");

        let assistant = ChatMessage::assistant(String::new(), Some(calls));
        let av = serde_json::to_value(&assistant).unwrap();
        assert_eq!(av["role"], "assistant");
        assert_eq!(av["tool_calls"][0]["function"]["name"], "math");

        let result = ChatMessage::tool_result("math".into(), "4".into());
        let rv = serde_json::to_value(&result).unwrap();
        assert_eq!(rv["role"], "tool");
        assert_eq!(rv["tool_name"], "math");
        assert_eq!(rv["content"], "4");
    }
}
