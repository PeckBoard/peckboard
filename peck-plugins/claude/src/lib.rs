//! Peckboard Claude CLI provider plugin (WASM / Extism).

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code, unused_imports))]

mod argv;
mod event;
mod host;
mod manifest;
mod models;
mod parser;
mod sandbox;
mod send;
mod settings;
mod usage;

use serde::Deserialize;

#[cfg(target_arch = "wasm32")]
mod entry {
    use super::*;
    use extism_pdk::*;

    #[plugin_fn]
    pub fn manifest() -> FnResult<String> {
        Ok(crate::manifest::manifest_json())
    }

    #[plugin_fn]
    pub fn init(_config: String) -> FnResult<String> {
        Ok(serde_json::json!({ "ok": true }).to_string())
    }

    #[plugin_fn]
    pub fn shutdown() -> FnResult<String> {
        Ok(serde_json::json!({ "ok": true }).to_string())
    }

    #[plugin_fn]
    pub fn handle(input: String) -> FnResult<String> {
        let call: HookCall = serde_json::from_str(&input)?;
        Ok(dispatch_hook(&call.hook, call.payload))
    }
}

/// The `{ "hook", "payload" }` envelope core passes to `handle`.
#[derive(Debug, Deserialize)]
struct HookCall {
    hook: String,
    #[serde(default)]
    payload: serde_json::Value,
}

fn dispatch_hook(hook: &str, payload: serde_json::Value) -> String {
    match hook {
        "provider.register" => handle_register(),
        "provider.send" => handle_send(payload),
        // Serve the catalog captured at register; a later implementation
        // can Allow a fresh list here.
        "provider.models" => handle_models(),
        // Cooperative stop is the host-side flag; this hook is cleanup only.
        "provider.interrupt" => skip(),
        _ => skip(),
    }
}

pub fn registration() -> serde_json::Value {
    serde_json::json!({
        "id": models::PROVIDER_ID,
        "display_name": models::DISPLAY_NAME,
        "models": models::seed_models(),
        "effort_levels": [
            { "id": "low", "label": "Low" },
            { "id": "medium", "label": "Medium" },
            { "id": "high", "label": "High" },
            { "id": "xhigh", "label": "Extra high" },
            { "id": "max", "label": "Max" },
        ],
        "supports_mid_stream_injection": true,
        "capabilities": {
            "supports_thinking": true,
            "supports_images_in": true,
            "supports_usage": true,
            "supports_resume": true,
            "interrupt_kind": "soft",
            "supports_mid_stream_injection": true,
            "answer_transport": "stdin",
        },
    })
}

fn handle_register() -> String {
    match host::call_host(host::HostFn::RegisterProvider, &registration()) {
        Ok(_) => allow(serde_json::json!({ "ok": true })),
        Err(e) => cancel(&e),
    }
}
/// Kill-switch for CLI model discovery. Set `PECKBOARD_CLAUDE_MODEL_DISCOVERY`
/// to `0`/`false`/`off` to always serve the static seed — the e2e harness
/// uses this to keep model labels deterministic on machines that have a real
/// `claude` binary installed. WASM builds have no environment; discovery
/// stays governed by the `discover_models` setting alone there.
fn env_discovery_enabled() -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        !matches!(
            std::env::var("PECKBOARD_CLAUDE_MODEL_DISCOVERY").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    }
    #[cfg(target_arch = "wasm32")]
    {
        true
    }
}

fn handle_models() -> String {
    let cli = settings::cli_path("claude");
    let extra = settings::str_list("additional_models");
    let discovered: Vec<String> = Vec::new();
    if settings::bool("discover_models", true) && env_discovery_enabled() {
        if let Some(out) = settings::probe(&cli, &["--version"]) {
            let _ = out;
        }
    }
    let models = settings::merge_catalog(
        models::seed_models(),
        discovered,
        extra,
        &settings::accounts(),
        |id| id.to_string(),
    );
    allow(serde_json::json!({ "models": models }))
}

fn handle_send(payload: serde_json::Value) -> String {
    match send::run(&payload) {
        Ok(()) => allow(serde_json::json!({ "ok": true })),
        Err(e) => cancel(&e),
    }
}

fn allow(value: serde_json::Value) -> String {
    serde_json::json!({ "verdict": "allow", "payload": value }).to_string()
}

fn cancel(reason: &str) -> String {
    serde_json::json!({ "verdict": "cancel", "reason": reason }).to_string()
}

fn skip() -> String {
    serde_json::json!({ "verdict": "skip" }).to_string()
}

pub fn send_turn(payload: &serde_json::Value) -> Result<(), String> {
    send::run(payload)
}

pub fn refresh_models() -> Option<serde_json::Value> {
    let out = handle_models();
    let v: serde_json::Value = serde_json::from_str(&out).ok()?;
    if v.get("verdict").and_then(|x| x.as_str()) != Some("allow") {
        return None;
    }
    v.get("payload")?.get("models").cloned()
}

pub fn manifest_json() -> String {
    manifest::manifest_json()
}
