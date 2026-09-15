//! Peckboard AI-provider plugin (WASM / Extism).
//!
//! Registers a provider on `provider.register`, then drives one turn per
//! `provider.send` call through the scripted scenarios in `send` (a sync port
//! of `src/provider/mock/mod.rs::run_scenario`). `provider.models` and
//! `provider.interrupt` skip so core serves the registered catalog and the
//! cooperative stop flag.
//!
//! ## Plugin interface
//!
//! Core expects four exports (`peckboard/src/plugin/manager.rs`):
//! - `manifest` — hooks, permissions, settings.
//! - `init` — called once on load; a no-op here.
//! - `handle` — called per hook with `{ "hook", "payload" }`; returns a Verdict.
//! - `shutdown` — teardown; a no-op here.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code, unused_imports))]

mod host;
mod manifest;
mod models;
mod send;

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
        "provider.models" => skip(),
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
        "pricing": {
            "echo": { "input_usd_per_mtok": 0.1, "output_usd_per_mtok": 0.5 },
            "happy-path": { "input_usd_per_mtok": 1.0, "output_usd_per_mtok": 5.0 },
        },
        "capabilities": {
            "supports_thinking": true,
            "supports_images_in": true,
            "supports_usage": true,
            "supports_resume": true,
            "interrupt_kind": "hard_kill",
            "supports_mid_stream_injection": false,
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

fn handle_send(payload: serde_json::Value) -> String {
    match send::run_scenario(&payload) {
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
    send::run_scenario(payload)
}

pub fn refresh_models() -> Option<serde_json::Value> {
    None
}

pub fn manifest_json() -> String {
    manifest::manifest_json()
}
