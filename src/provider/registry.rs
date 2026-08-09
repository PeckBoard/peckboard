use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::agent::AgentProvider;
use super::stream::ModelInfo;

/// One selectable reasoning-effort level a provider exposes.
///
/// `id` is the raw value handed to the provider (e.g. the CLI `--effort`
/// flag); `label` is the human-facing name shown in the effort picker.
/// Every provider supplies its own set — Claude and Grok expose the full
/// ladder, Cursor bakes effort into the model id, and Ollama/Mock have
/// none — so the UI can load a model's provider's levels the moment a
/// model is chosen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffortLevel {
    pub id: String,
    pub label: String,
}

/// The standard reasoning-effort ladder shared by the Claude and Grok CLIs:
/// `low`, `medium`, `high`, `xhigh` (Extra high), `max`. Providers that
/// support the same set reuse this so the ladder is defined once.
pub fn standard_effort_levels() -> Vec<EffortLevel> {
    [
        ("low", "Low"),
        ("medium", "Medium"),
        ("high", "High"),
        ("xhigh", "Extra high"),
        ("max", "Max"),
    ]
    .into_iter()
    .map(|(id, label)| EffortLevel {
        id: id.into(),
        label: label.into(),
    })
    .collect()
}
/// How a provider's `interrupt()` actually stops a run — drives the UI's
/// interrupt affordance (label + tooltip) so "Interrupt" is only promised
/// where the provider can settle a turn in-band.
///
/// - `Soft`: in-band interrupt; the agent stops cleanly mid-turn and the
///   session process stays usable (Claude CLI control_request, with a
///   hard-kill fallback).
/// - `Cooperative`: a stop flag the run polls between chunks; the turn
///   halts at the next safe point (WASM plugin providers).
/// - `HardKill`: the in-flight run/process is terminated outright
///   (per-turn CLI and HTTP providers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InterruptKind {
    Soft,
    #[default]
    Cooperative,
    HardKill,
}

/// How an answer to an agent question (`ControlRequest`) reaches the run:
/// written to the live run's stdin channel mid-turn, or dispatched as a
/// fresh turn once the current one ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AnswerTransport {
    Stdin,
    #[default]
    NewTurn,
}

/// What a provider actually supports, declared at registration and served
/// verbatim through `/api/models` + the MCP `list_models` tool so the UI
/// can gate affordances instead of rendering every provider as if it were
/// Claude.
///
/// The serde field defaults double as the conservative defaults for
/// plugin-registered providers that omit the optional `capabilities`
/// field ([`Default`] matches them): promise nothing the
/// `PluginProviderAdapter` can't deliver. Native providers always
/// construct the struct explicitly. The WEB fallback for a provider
/// entry missing capabilities entirely is the opposite — today's
/// Claude-shaped assumptions — so old payloads keep current behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    /// At least one of the provider's models can emit extended reasoning
    /// ("thinking"). Per-model truth stays on `ModelInfo::is_thinking`.
    #[serde(default)]
    pub supports_thinking: bool,
    /// Image attachments on a user turn reach the model. `false` ⇒ the
    /// provider drops them, so the UI disables the attach button. Vision
    /// support can additionally be gated per model (see
    /// `ModelInfo::images_in_hint`).
    #[serde(default = "default_true")]
    pub supports_images_in: bool,
    /// The provider emits per-turn `Usage` events (token counts feed the
    /// usage tables and rollups).
    #[serde(default)]
    pub supports_usage: bool,
    /// A conversation can be resumed across turns/restarts (a
    /// `conversation_id` round-trips, or the provider replays history).
    #[serde(default = "default_true")]
    pub supports_resume: bool,
    #[serde(default)]
    pub interrupt_kind: InterruptKind,
    /// Mirrors `AgentProvider::supports_mid_stream_injection`: a second
    /// user message mid-turn is consumed by the same live run instead of
    /// being queued for the next turn.
    #[serde(default)]
    pub supports_mid_stream_injection: bool,
    #[serde(default)]
    pub answer_transport: AnswerTransport,
}

fn default_true() -> bool {
    true
}

impl Default for ProviderCapabilities {
    /// Conservative plugin-provider defaults — identical to the serde
    /// field defaults above.
    fn default() -> Self {
        ProviderCapabilities {
            supports_thinking: false,
            supports_images_in: true,
            supports_usage: false,
            supports_resume: true,
            interrupt_kind: InterruptKind::Cooperative,
            supports_mid_stream_injection: false,
            answer_transport: AnswerTransport::NewTurn,
        }
    }
}

impl ProviderCapabilities {
    /// Defaults for a plugin registration that omitted `capabilities`:
    /// the conservative baseline, with `supports_thinking` derived from
    /// the registered model catalog's capability tags.
    pub fn plugin_defaults(models: &[ModelInfo]) -> Self {
        ProviderCapabilities {
            supports_thinking: models.iter().any(|m| m.is_thinking()),
            ..Default::default()
        }
    }

    /// Effective image-input answer for one of this provider's models,
    /// as served per model in `/api/models` / `list_models`: a provider
    /// that drops images wins (`Some(false)`); otherwise the model's own
    /// capability-tag hint; `None` = unknown, callers keep the permissive
    /// provider-level default.
    pub fn model_images_in(&self, model: &ModelInfo) -> Option<bool> {
        if !self.supports_images_in {
            Some(false)
        } else {
            model.images_in_hint()
        }
    }
}

/// Registered provider metadata.
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    pub id: String,
    pub display_name: String,
    pub models: Vec<ModelInfo>,
    /// Effort levels this provider exposes for the effort picker. Empty when
    /// the provider has no reasoning-effort control (e.g. Ollama, or Cursor
    /// where effort is baked into the model id). The UI always prepends a
    /// "Default" option, so an empty list means "Default only".
    pub effort_levels: Vec<EffortLevel>,
    /// What this provider actually supports — served to the UI so it can
    /// gate affordances (attach button, interrupt label, thinking UI).
    pub capabilities: ProviderCapabilities,
}

struct RegisteredProvider {
    info: ProviderInfo,
    provider: Arc<dyn AgentProvider>,
}

/// Registry of all available AI providers and their models.
///
/// Holds both the metadata (for `/api/models`) and the trait object that
/// the dispatcher uses to actually drive a run.
pub struct ProviderRegistry {
    providers: Mutex<HashMap<String, RegisteredProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        ProviderRegistry {
            providers: Mutex::new(HashMap::new()),
        }
    }

    /// Register a provider implementation along with its metadata.
    /// Overwrites if the same ID already exists.
    pub async fn register(&self, provider: Arc<dyn AgentProvider>, info: ProviderInfo) {
        let mut providers = self.providers.lock().await;
        tracing::info!(
            "Registered provider '{}' ({}) with {} models",
            info.id,
            info.display_name,
            info.models.len()
        );
        providers.insert(info.id.clone(), RegisteredProvider { info, provider });
    }

    /// Remove a provider by ID (e.g. a plugin-registered provider whose
    /// plugin was unloaded, denied, or uninstalled). Returns whether an
    /// entry existed. In-flight runs keep their `Arc<dyn AgentProvider>`
    /// alive; only new lookups stop resolving.
    pub async fn unregister(&self, id: &str) -> bool {
        let mut providers = self.providers.lock().await;
        let removed = providers.remove(id).is_some();
        if removed {
            tracing::info!("Unregistered provider '{id}'");
        }
        removed
    }
    /// Get provider metadata by ID.
    pub async fn get_info(&self, id: &str) -> Option<ProviderInfo> {
        let providers = self.providers.lock().await;
        providers.get(id).map(|r| r.info.clone())
    }

    /// Get the provider implementation by ID.
    pub async fn get_provider(&self, id: &str) -> Option<Arc<dyn AgentProvider>> {
        let providers = self.providers.lock().await;
        providers.get(id).map(|r| r.provider.clone())
    }

    /// List all registered providers' metadata, with the **static** model
    /// list captured at init. Cheap (no provider calls) — this is the form
    /// used by the dispatch/fan-out paths that only need provider ids.
    /// Use [`list_providers_with_models`](Self::list_providers_with_models)
    /// for the UI catalog, where settings-derived models must be resolved.
    pub async fn list_providers(&self) -> Vec<ProviderInfo> {
        let providers = self.providers.lock().await;
        providers.values().map(|r| r.info.clone()).collect()
    }

    /// List all providers with their **effective** model list: a
    /// provider's [`dynamic_models`](super::agent::AgentProvider::dynamic_models)
    /// override (settings-derived, e.g. Ollama's user-registered extras)
    /// when it supplies one, else the static list from `ProviderInfo`.
    ///
    /// This is the catalog form the `/api/models` route and the MCP
    /// `list_models` tool consume so a settings change shows up without a
    /// restart. `dynamic_models()` can be slow — the Claude provider
    /// probes its CLI with a ~10s cap on a discovery-cache miss — so the
    /// registry lock is released BEFORE the calls: holding it would stall
    /// `get_provider` (send/interrupt/cancel paths) app-wide for the
    /// whole probe.
    pub async fn list_providers_with_models(&self) -> Vec<ProviderInfo> {
        self.list_providers_with_models_except(&std::collections::HashSet::new())
            .await
    }

    /// [`list_providers_with_models`](Self::list_providers_with_models)
    /// minus the providers in `exclude` (the hidden/disabled set from
    /// Settings → Providers & Accounts). Excluded providers are skipped
    /// BEFORE `dynamic_models()` runs, so a disabled provider is never
    /// probed for its catalog (Ollama HTTP discovery, CLI probes, …).
    pub async fn list_providers_with_models_except(
        &self,
        exclude: &std::collections::HashSet<String>,
    ) -> Vec<ProviderInfo> {
        let entries: Vec<(ProviderInfo, Arc<dyn AgentProvider>)> = {
            let providers = self.providers.lock().await;
            providers
                .values()
                .filter(|r| !exclude.contains(&r.info.id))
                .map(|r| (r.info.clone(), r.provider.clone()))
                .collect()
        };
        let mut out = Vec::with_capacity(entries.len());
        for (info, provider) in entries {
            let models = match provider.dynamic_models().await {
                Some(models) => models,
                None => info.models.clone(),
            };
            out.push(ProviderInfo { models, ..info });
        }
        out
    }

    /// [`list_providers_with_models_except`](Self::list_providers_with_models_except)
    /// but each provider's catalog is the UNION of its effective (dynamic)
    /// list and its static seed: dynamic entries first, then any static model
    /// the probe dropped, deduped by model id. `dynamic_models()` REPLACES the
    /// static seed on purpose for the picker (`/api/models`) — but that also
    /// hides a seed model an outdated external catalog doesn't advertise yet
    /// (the Claude CLI's initialize handshake omitting `claude-fable-5`, say).
    /// Plugin-facing listings use the union so every registry-known model
    /// stays selectable.
    pub async fn list_providers_with_models_union_except(
        &self,
        exclude: &std::collections::HashSet<String>,
    ) -> Vec<ProviderInfo> {
        let entries: Vec<(ProviderInfo, Arc<dyn AgentProvider>)> = {
            let providers = self.providers.lock().await;
            providers
                .values()
                .filter(|r| !exclude.contains(&r.info.id))
                .map(|r| (r.info.clone(), r.provider.clone()))
                .collect()
        };
        let mut out = Vec::with_capacity(entries.len());
        for (info, provider) in entries {
            let models = match provider.dynamic_models().await {
                Some(mut dynamic) => {
                    for m in &info.models {
                        if !dynamic.iter().any(|d| d.id == m.id) {
                            dynamic.push(m.clone());
                        }
                    }
                    dynamic
                }
                None => info.models.clone(),
            };
            out.push(ProviderInfo { models, ..info });
        }
        out
    }

    /// Best-effort auth status per provider id, for the model picker's
    /// "not configured" hint (see [`AgentProvider::auth_configured`]).
    /// Same locking discipline as `list_providers_with_models`: the
    /// registry lock is released before the async provider calls.
    pub async fn provider_auth_status(&self) -> std::collections::HashMap<String, Option<bool>> {
        let entries: Vec<(String, Arc<dyn AgentProvider>)> = {
            let providers = self.providers.lock().await;
            providers
                .values()
                .map(|r| (r.info.id.clone(), r.provider.clone()))
                .collect()
        };
        let mut out = std::collections::HashMap::with_capacity(entries.len());
        for (id, provider) in entries {
            out.insert(id, provider.auth_configured().await);
        }
        out
    }
    /// Provider candidates for auto-model resolution: id, best-effort auth
    /// status, and the STATIC model catalog (not `dynamic_models` — that can
    /// probe a CLI and is too slow for the dispatch hot path this feeds).
    /// Excludes the `mock` provider: it is the scripted dev/test vehicle and
    /// must never be auto-routed to for real work. Also excludes any id in
    /// `exclude` — the hidden/disabled set from Settings → Providers &
    /// Accounts — so auto-routing never lands on a provider the user hid.
    pub async fn auto_model_candidates(
        &self,
        exclude: &std::collections::HashSet<String>,
    ) -> Vec<crate::provider::AutoCandidate> {
        let providers = self.providers.lock().await;
        let mut out = Vec::with_capacity(providers.len());
        for r in providers.values() {
            if r.info.id == "mock" || exclude.contains(&r.info.id) {
                continue;
            }
            let auth = r.provider.auth_configured().await;
            out.push(crate::provider::AutoCandidate {
                provider_id: r.info.id.clone(),
                auth,
                models: r.info.models.clone(),
            });
        }
        out
    }

    /// List all models across all providers, with provider:model format
    /// IDs. Resolves each provider's effective (dynamic-or-static) model
    /// list, so settings-derived models are included.
    pub async fn list_all_models(&self) -> Vec<(String, ModelInfo)> {
        let providers = self.list_providers_with_models().await;
        let mut models = Vec::new();
        for info in &providers {
            for model in &info.models {
                let full_id = format!("{}:{}", info.id, model.id);
                models.push((full_id, model.clone()));
            }
        }
        models
    }

    /// Whether the given model id resolves to a thinking (reasoning) model.
    /// Gates planning. Unknown models return `false` (planning is refused
    /// rather than risked). Accepts `provider:model`, bare `model`, and an
    /// optional `@account` suffix.
    pub async fn is_thinking_model(&self, model_id: &str) -> bool {
        let (base, _account) = split_model_account(model_id);
        let (provider_id, model) = Self::parse_model_id(base, "claude");
        // Resolve only the addressed provider's effective catalog — going
        // through list_providers_with_models() here ran every provider's
        // dynamic_models() probe (disabled ones included) to answer a
        // single-provider question.
        let Some((info, provider)) = ({
            let providers = self.providers.lock().await;
            providers
                .get(provider_id.as_str())
                .map(|r| (r.info.clone(), r.provider.clone()))
        }) else {
            return false;
        };
        let models = match provider.dynamic_models().await {
            Some(models) => models,
            None => info.models,
        };
        models
            .iter()
            .find(|m| m.id == model)
            .is_some_and(|m| m.is_thinking())
    }
    /// The cheapest model `provider_id` offers, ranked by the provider's own
    /// published price (input + output USD per million tokens, via
    /// `AgentProvider::model_price`). `None` when the provider is unknown or
    /// prices none of its models — an unpriced model is unknown, never free.
    /// Ties keep the earlier catalog entry.
    pub async fn cheapest_model(&self, provider_id: &str) -> Option<String> {
        let (info, provider) = {
            let providers = self.providers.lock().await;
            let r = providers.get(provider_id)?;
            (r.info.clone(), r.provider.clone())
        };
        let models = match provider.dynamic_models().await {
            Some(models) => models,
            None => info.models,
        };
        let mut best: Option<(String, f64)> = None;
        for m in &models {
            if let Some((input, output)) = provider.model_price(&m.id) {
                let total = input + output;
                if best.as_ref().map_or(true, |(_, b)| total < *b) {
                    best = Some((m.id.clone(), total));
                }
            }
        }
        best.map(|(id, _)| id)
    }

    /// Parse a model ID. Returns (provider_id, model_id).
    /// If no prefix, uses the default provider.
    pub fn parse_model_id(model_id: &str, default_provider: &str) -> (String, String) {
        match model_id.split_once(':') {
            Some((provider, model)) => (provider.to_string(), model.to_string()),
            None => (default_provider.to_string(), model_id.to_string()),
        }
    }
}

/// Split a (possibly account-scoped) model id into its base model and the
/// Claude account id it targets.
///
/// Multi-account support folds the account into the model id with an `@`
/// suffix: `claude:claude-opus-4-8@acc_1a2b`. A model id with no `@` — every
/// session/card stored before multi-account, plus the implicit "Default"
/// account — yields `(model, None)`. `@` never appears in a Claude model id
/// or a Bedrock ARN, and account ids are generated without it, so the split
/// is unambiguous. Works on a full `claude:<model>@<acct>` or a bare
/// `<model>@<acct>` — the provider prefix carries no `@` either.
pub fn split_model_account(model_id: &str) -> (&str, Option<&str>) {
    match model_id.rsplit_once('@') {
        Some((model, acct)) if !acct.is_empty() => (model, Some(acct)),
        _ => (model_id, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::mock::MockProvider;

    #[tokio::test]
    async fn test_register_and_list() {
        let registry = ProviderRegistry::new();

        registry
            .register(
                Arc::new(MockProvider::new()),
                ProviderInfo {
                    id: "claude".into(),
                    display_name: "Claude".into(),
                    models: vec![
                        ModelInfo {
                            id: "opus".into(),
                            display_name: "Claude Opus".into(),
                            capabilities: vec!["code".into(), "reasoning".into()],
                            tier: 3,
                        },
                        ModelInfo {
                            id: "sonnet".into(),
                            display_name: "Claude Sonnet".into(),
                            capabilities: vec!["code".into()],
                            tier: 2,
                        },
                    ],
                    effort_levels: standard_effort_levels(),
                    capabilities: ProviderCapabilities::default(),
                },
            )
            .await;

        let providers = registry.list_providers().await;
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].models.len(), 2);

        let claude = registry.get_info("claude").await;
        assert!(claude.is_some());

        let missing = registry.get_info("openai").await;
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn test_list_all_models() {
        let registry = ProviderRegistry::new();

        registry
            .register(
                Arc::new(MockProvider::new()),
                ProviderInfo {
                    id: "claude".into(),
                    display_name: "Claude".into(),
                    models: vec![ModelInfo {
                        id: "opus".into(),
                        display_name: "Opus".into(),
                        capabilities: vec![],
                        tier: 3,
                    }],
                    effort_levels: vec![],
                    capabilities: ProviderCapabilities::default(),
                },
            )
            .await;

        let models = registry.list_all_models().await;
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].0, "claude:opus");
    }

    #[test]
    fn test_parse_model_id() {
        let (p, m) = ProviderRegistry::parse_model_id("claude:opus", "claude");
        assert_eq!(p, "claude");
        assert_eq!(m, "opus");

        let (p, m) = ProviderRegistry::parse_model_id("opus", "claude");
        assert_eq!(p, "claude");
        assert_eq!(m, "opus");

        let (p, m) = ProviderRegistry::parse_model_id("openai:gpt-4o", "claude");
        assert_eq!(p, "openai");
        assert_eq!(m, "gpt-4o");
    }

    #[test]
    fn test_split_model_account() {
        // Account-scoped: base model + account id.
        assert_eq!(
            split_model_account("claude-opus-4-8@acc_1a2b"),
            ("claude-opus-4-8", Some("acc_1a2b"))
        );
        // Works on the full provider-prefixed form too.
        assert_eq!(
            split_model_account("claude:claude-opus-4-8@acc_1a2b"),
            ("claude:claude-opus-4-8", Some("acc_1a2b"))
        );
        // No suffix → Default account (backward compatible).
        assert_eq!(
            split_model_account("claude:claude-opus-4-8"),
            ("claude:claude-opus-4-8", None)
        );
        // A trailing `@` with no id is treated as no account, not an empty one.
        assert_eq!(
            split_model_account("claude-opus-4-8@"),
            ("claude-opus-4-8@", None)
        );
        // A Bedrock ARN (colons, slashes, no `@`) is left whole.
        assert_eq!(
            split_model_account("arn:aws:bedrock:us-east-1::model/x"),
            ("arn:aws:bedrock:us-east-1::model/x", None)
        );
    }

    #[tokio::test]
    async fn cheapest_model_ranks_by_provider_price() {
        let registry = ProviderRegistry::new();
        registry
            .register(
                Arc::new(MockProvider::new()),
                ProviderInfo {
                    id: "mock".into(),
                    display_name: "Mock".into(),
                    models: crate::provider::mock::mock_model_infos(),
                    effort_levels: vec![],
                    capabilities: ProviderCapabilities::default(),
                },
            )
            .await;

        // `echo` (0.1 + 0.5) undercuts `happy-path` (1.0 + 5.0); the
        // unpriced scenarios never win even though they'd sort "free".
        assert_eq!(
            registry.cheapest_model("mock").await.as_deref(),
            Some("echo")
        );
        // Unknown provider → no answer.
        assert_eq!(registry.cheapest_model("nope").await, None);
    }

    #[tokio::test]
    async fn auto_model_candidates_excludes_hidden_providers() {
        let registry = ProviderRegistry::new();
        registry
            .register(
                Arc::new(MockProvider::new()),
                ProviderInfo {
                    id: "claude".into(),
                    display_name: "Claude".into(),
                    models: vec![ModelInfo {
                        id: "sonnet".into(),
                        display_name: "Sonnet".into(),
                        capabilities: vec![],
                        tier: 2,
                    }],
                    effort_levels: standard_effort_levels(),
                    capabilities: ProviderCapabilities::default(),
                },
            )
            .await;
        registry
            .register(
                Arc::new(MockProvider::new()),
                ProviderInfo {
                    id: "ollama".into(),
                    display_name: "Ollama".into(),
                    models: vec![ModelInfo {
                        id: "llama3".into(),
                        display_name: "Llama 3".into(),
                        capabilities: vec![],
                        tier: 1,
                    }],
                    effort_levels: standard_effort_levels(),
                    capabilities: ProviderCapabilities::default(),
                },
            )
            .await;

        let hidden: std::collections::HashSet<String> = ["claude".to_string()].into();
        let candidates = registry.auto_model_candidates(&hidden).await;
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].provider_id, "ollama");

        let resolved = crate::provider::resolve_auto_model(&candidates, None, false).unwrap();
        assert_eq!(resolved, "ollama:llama3");
    }

    #[tokio::test]
    async fn auto_model_candidates_all_hidden_yields_empty() {
        let registry = ProviderRegistry::new();
        registry
            .register(
                Arc::new(MockProvider::new()),
                ProviderInfo {
                    id: "claude".into(),
                    display_name: "Claude".into(),
                    models: vec![ModelInfo {
                        id: "sonnet".into(),
                        display_name: "Sonnet".into(),
                        capabilities: vec![],
                        tier: 2,
                    }],
                    effort_levels: standard_effort_levels(),
                    capabilities: ProviderCapabilities::default(),
                },
            )
            .await;

        let hidden: std::collections::HashSet<String> = ["claude".to_string()].into();
        let candidates = registry.auto_model_candidates(&hidden).await;
        assert!(candidates.is_empty());

        let err = crate::provider::resolve_auto_model(&candidates, None, false).unwrap_err();
        assert!(err.to_string().contains("Settings"));
    }

    /// A provider whose dynamic probe returns a SUBSET of its static seed —
    /// the shape of a stale external catalog hiding a seed model.
    struct ProbeProvider(Vec<ModelInfo>);

    #[async_trait::async_trait]
    impl crate::provider::agent::AgentProvider for ProbeProvider {
        fn id(&self) -> &str {
            "probe"
        }
        async fn dynamic_models(&self) -> Option<Vec<ModelInfo>> {
            Some(self.0.clone())
        }
        async fn send_message(
            &self,
            _ctx: crate::provider::agent::SendMessageContext,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn cancel(&self, _session_id: &str) {}
        async fn interrupt(&self, _session_id: &str) {}
        async fn write_stdin(&self, _session_id: &str, _text: &str) -> bool {
            false
        }
        async fn is_running(&self, _session_id: &str) -> bool {
            false
        }
        async fn cleanup(&self) {}
        async fn shutdown(&self) {}
    }

    #[tokio::test]
    async fn union_listing_restores_static_models_a_probe_dropped() {
        let registry = ProviderRegistry::new();
        let m = |id: &str, caps: &[&str], tier: i32| ModelInfo {
            id: id.into(),
            display_name: id.into(),
            capabilities: caps.iter().map(|c| c.to_string()).collect(),
            tier,
        };
        registry
            .register(
                Arc::new(ProbeProvider(vec![m(
                    "old-model",
                    &["code", "reasoning"],
                    3,
                )])),
                ProviderInfo {
                    id: "probe".into(),
                    display_name: "Probe".into(),
                    models: vec![
                        m("old-model", &["code", "reasoning"], 3),
                        m("new-model", &["code", "reasoning"], 4),
                    ],
                    effort_levels: vec![],
                    capabilities: ProviderCapabilities::default(),
                },
            )
            .await;

        // The effective list drops `new-model`; the union restores it.
        let effective = registry
            .list_providers_with_models_except(&std::collections::HashSet::new())
            .await;
        assert!(!effective[0].models.iter().any(|m| m.id == "new-model"));

        let union = registry
            .list_providers_with_models_union_except(&std::collections::HashSet::new())
            .await;
        let ids: Vec<&str> = union[0].models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["old-model", "new-model"]);
    }
}
