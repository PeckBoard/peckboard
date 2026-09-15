//! In-process AgentProvider for first-party crate plugins.
//!
//! Reuses each plugin crate's `send` loop (the same code the WASM build
//! runs) by installing a thread-local host dispatcher for the duration of
//! one `spawn_blocking` turn. Third-party providers still go through
//! [`super::plugin_provider::PluginProviderAdapter`].

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use async_trait::async_trait;

use crate::db::Db;
use crate::plugin::host;
use crate::plugin::manager::PluginManager;
use crate::plugin::settings::SettingsSchema;
use crate::provider::agent::{AgentProvider, ProcessCompletion, SendMessageContext, emit_event};
use crate::provider::plugin_provider::{
    PluginProviderRuntime, ProviderRegistration, SessionSnapshot, Terminal, TurnState,
    effective_capabilities, list_accounts_json, message_payload, probe_cli_json,
    validate_refresh_models, validate_registration,
};
use crate::provider::registry::{ProviderInfo, ProviderRegistry};
use crate::provider::stream::{CrashKind, ModelInfo, ProviderEvent};
use crate::provider::turn::compose_system_prompt;

/// Function pointers into one first-party provider crate.
#[derive(Clone, Copy)]
pub struct CrateHooks {
    pub send: fn(&serde_json::Value) -> Result<(), String>,
    pub refresh_models: fn() -> Option<serde_json::Value>,
    pub registration: fn() -> serde_json::Value,
    pub manifest_json: fn() -> String,
}

/// Settings schema declared in the crate's plugin manifest, or empty when
/// the manifest has no `settings` array.
pub fn settings_schema(hooks: CrateHooks) -> SettingsSchema {
    let json = (hooks.manifest_json)();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
    let fields = v.get("settings").cloned().unwrap_or(serde_json::json!([]));
    serde_json::from_value(fields)
        .map(SettingsSchema::new)
        .unwrap_or_default()
}

/// Register `hooks` as a native [`AgentProvider`]. No-op (with a warning)
/// when the crate's registration JSON fails validation.
pub async fn register_crate_provider(
    provider_registry: &ProviderRegistry,
    plugins: Arc<PluginManager>,
    db: &Db,
    hooks: CrateHooks,
) {
    let body = (hooks.registration)();
    let reg: ProviderRegistration = match serde_json::from_value(body) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("crate provider registration JSON invalid: {e}");
            return;
        }
    };
    if let Err(e) = validate_registration(&reg) {
        tracing::error!(provider = %reg.id, "crate provider registration invalid: {e}");
        return;
    }
    let adapter = Arc::new(CrateAgentProvider::new(&reg, plugins, db.clone(), hooks));
    let info = ProviderInfo {
        id: reg.id.clone(),
        display_name: reg.display_name.clone(),
        models: reg.models.clone(),
        effort_levels: reg.effort_levels.clone(),
        capabilities: effective_capabilities(&reg),
    };
    provider_registry.register(adapter, info).await;
}

struct CrateAgentProvider {
    provider_id: String,
    plugin_id: String,
    runtime: Arc<PluginProviderRuntime>,
    plugins: Arc<PluginManager>,
    db: Db,
    hooks: CrateHooks,
    pricing: HashMap<String, (f64, f64)>,
    mid_stream: bool,
}

impl CrateAgentProvider {
    fn new(
        registration: &ProviderRegistration,
        plugins: Arc<PluginManager>,
        db: Db,
        hooks: CrateHooks,
    ) -> Self {
        Self {
            provider_id: registration.id.clone(),
            plugin_id: registration.id.clone(),
            runtime: plugins.provider_runtime(),
            plugins,
            db,
            hooks,
            mid_stream: registration.supports_mid_stream_injection,
            pricing: registration
                .pricing
                .iter()
                .map(|(m, p)| (m.clone(), (p.input_usd_per_mtok, p.output_usd_per_mtok)))
                .collect(),
        }
    }

    fn host_fn(&self) -> peck_plugin_native_host::HostFn {
        let runtime = self.runtime.clone();
        let plugin_id = self.plugin_id.clone();
        let db = self.db.clone();
        let plugins = self.plugins.clone();
        Arc::new(move |name: &str, input: &str| {
            dispatch_crate_host(&runtime, &plugin_id, &db, &plugins, name, input)
        })
    }
}

#[async_trait]
impl AgentProvider for CrateAgentProvider {
    fn id(&self) -> &str {
        &self.provider_id
    }

    fn model_price(&self, model_id: &str) -> Option<(f64, f64)> {
        self.pricing.get(model_id).copied()
    }
    /// The crate's `refresh_models` merges settings (`additional_models`,
    /// per-account variants) FRESH on every call so a settings or account
    /// change shows up without a restart; only the CLI discovery probe
    /// inside it is cached (host-side, in `probe_cli_json`).
    async fn dynamic_models(&self) -> Option<Vec<ModelInfo>> {
        let host = self.host_fn();
        let refresh = self.hooks.refresh_models;
        let raw =
            tokio::task::spawn_blocking(move || peck_plugin_native_host::with_host(host, refresh))
                .await
                .ok()??;
        let models: Vec<ModelInfo> = serde_json::from_value(raw).ok()?;
        if let Err(e) = validate_refresh_models(&models) {
            tracing::warn!(
                plugin = %self.plugin_id,
                "crate provider returned an invalid models catalog: {e}"
            );
            return None;
        }
        Some(models)
    }

    async fn send_message(&self, ctx: SendMessageContext) -> anyhow::Result<()> {
        if self.mid_stream
            && self.runtime.queue_injection(
                &self.plugin_id,
                &ctx.session_id,
                message_payload(&ctx.message),
            )
        {
            return Ok(());
        }

        let session = ctx
            .db
            .get_session(&ctx.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session not found: {}", ctx.session_id))?;
        let snapshot = SessionSnapshot {
            folder_path: ctx.config.working_dir.clone(),
            folder_id: session.folder_id.clone(),
            card_id: session.card_id,
            project_id: session.project_id,
            is_worker: ctx.config.is_worker,
            mcp_config_path: ctx.config.mcp_config_path.clone(),
        };
        self.runtime
            .begin_turn(
                &ctx.session_id,
                TurnState {
                    plugin_id: self.plugin_id.clone(),
                    stop: AtomicBool::new(false),
                    terminal: std::sync::Mutex::new(None),
                    db: ctx.db.clone(),
                    broadcaster: ctx.broadcaster.clone(),
                    rt: tokio::runtime::Handle::current(),
                    snapshot,
                    injected: std::sync::Mutex::new(std::collections::VecDeque::new()),
                    stdin_q: Default::default(),
                    plugins: Some(self.plugins.clone()),
                },
            )
            .map_err(|e| anyhow::anyhow!(e))?;

        let payload = serde_json::json!({
            "session_id": ctx.session_id,
            "provider_id": self.provider_id,
            "spawn_config": ctx.config,
            "message": message_payload(&ctx.message),
            "conversation_id": ctx.conversation_id.as_ref().map(|h| h.id()),
            "system_prompt": compose_system_prompt(&ctx.config),
        });

        let host = self.host_fn();
        let send = self.hooks.send;
        let runtime = self.runtime.clone();
        let session_id = ctx.session_id.clone();
        let db = ctx.db.clone();
        let broadcaster = ctx.broadcaster.clone();
        let completion_tx = ctx.completion_tx.clone();
        let run_id = ctx.run_id;
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                peck_plugin_native_host::with_host(host, || send(&payload))
            })
            .await;
            let result = match result {
                Ok(inner) => inner,
                Err(e) => Err(e.to_string()),
            };
            let terminal = runtime.end_turn(&session_id);
            let (completed, error, error_kind) = match terminal {
                Some(Terminal::Completed) => (true, None, None),
                Some(Terminal::Crashed { reason, kind }) => (false, Some(reason), Some(kind)),
                None => {
                    let reason = match result {
                        Err(e) => e,
                        Ok(()) => "crate provider returned without emitting Completed or Crashed"
                            .to_string(),
                    };
                    let error_kind = CrashKind::classify(&reason);
                    emit_event(
                        &db,
                        &broadcaster,
                        &session_id,
                        ProviderEvent::Crashed {
                            reason: reason.clone(),
                            error_kind,
                            exit_code: None,
                            stderr: None,
                        },
                    )
                    .await;
                    (false, Some(reason), Some(error_kind))
                }
            };
            let _ = completion_tx
                .send(ProcessCompletion {
                    session_id,
                    completed,
                    error,
                    run_id,
                    error_kind,
                    turn_end_only: false,
                })
                .await;
        });
        Ok(())
    }

    async fn cancel(&self, session_id: &str) {
        self.runtime.request_stop(session_id);
    }

    async fn interrupt(&self, session_id: &str) {
        self.runtime.request_stop(session_id);
    }

    async fn write_stdin(&self, session_id: &str, text: &str) -> bool {
        self.runtime.write_child_stdin(session_id, text)
    }

    fn supports_mid_stream_injection(&self) -> bool {
        self.mid_stream
    }

    async fn is_running(&self, session_id: &str) -> bool {
        self.runtime.is_active(session_id)
    }

    async fn wait_for_termination(&self, session_id: &str) {
        let wait = async {
            while self.runtime.is_active(session_id) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        };
        if tokio::time::timeout(std::time::Duration::from_secs(15), wait)
            .await
            .is_err()
        {
            tracing::warn!(session_id, "crate provider wait_for_termination timed out");
            self.runtime.request_stop(session_id);
        }
    }

    async fn cleanup(&self) {}

    async fn shutdown(&self) {
        self.runtime.request_stop_for_plugin(&self.plugin_id);
    }
}

fn dispatch_crate_host(
    runtime: &PluginProviderRuntime,
    plugin_id: &str,
    db: &Db,
    plugins: &PluginManager,
    name: &str,
    input: &str,
) -> String {
    match name {
        "peckboard_register_provider" => serde_json::json!({ "ok": true }).to_string(),
        "peckboard_emit_provider_event" => runtime.emit_from_plugin(plugin_id, input),
        "peckboard_provider_should_stop" => runtime.should_stop_json(plugin_id, input),
        "peckboard_provider_take_message" => runtime.take_message_json(plugin_id, input),
        "peckboard_provider_get_session" => runtime.get_session_json(plugin_id, input),
        "peckboard_provider_get_mcp_config" => runtime.get_mcp_config_json(plugin_id, input),
        "peckboard_provider_account_env" => runtime.account_env_json(plugin_id, input),
        "peckboard_provider_write_file" => runtime.write_file_json(plugin_id, input),
        "peckboard_provider_spawn" => runtime.spawn_json(plugin_id, input),
        "peckboard_provider_read_line" => runtime.read_line_json(plugin_id, input),
        "peckboard_provider_write_stdin" => runtime.write_stdin_json(plugin_id, input),
        "peckboard_provider_read_stdin" => runtime.take_stdin_json(plugin_id, input),
        "peckboard_provider_kill" => runtime.kill_json(plugin_id, input),
        "peckboard_provider_probe" => probe_cli_json(input),
        "peckboard_provider_list_accounts" => list_accounts_json(db, plugin_id),
        "peckboard_provider_invoke_mcp" => runtime.invoke_mcp_json(plugin_id, input),
        "peckboard_get_plugin_setting" => host::get_plugin_setting_impl(db, plugin_id, input),
        "peckboard_http_request" => host::http_request_impl(input),
        "peckboard_store_put" => {
            host::store_put_impl(db, plugin_id, input, plugins.live_host().as_deref())
        }
        "peckboard_store_get" => host::store_get_impl(db, plugin_id, input),
        "peckboard_store_list" => host::store_list_impl(db, plugin_id, input),
        "peckboard_store_delete" => {
            host::store_delete_impl(db, plugin_id, input, plugins.live_host().as_deref())
        }
        other => {
            serde_json::json!({ "error": format!("unknown host function {other}") }).to_string()
        }
    }
}
