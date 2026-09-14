//! Peckboard AI-provider plugin (WASM / Extism).
//!
//! Registers a provider on `provider.register`, then drives one turn per
//! `provider.send` call. Send is a stub: it emits Started, Text("not
//! implemented"), and Completed. `provider.models` and `provider.interrupt`
//! skip so core serves the registered catalog and the cooperative stop flag.
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

fn handle_register() -> String {
    let body = serde_json::json!({
        "id": models::PROVIDER_ID,
        "display_name": models::DISPLAY_NAME,
        "models": models::seed_models(),
        "effort_levels": [],
    });
    match host::call_host(host::HostFn::RegisterProvider, &body) {
        Ok(_) => allow(serde_json::json!({ "ok": true })),
        Err(e) => cancel(&e),
    }
}

fn handle_send(payload: serde_json::Value) -> String {
    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if session_id.is_empty() {
        return cancel("provider.send payload missing session_id");
    }
    let model = payload
        .get("spawn_config")
        .and_then(|c| c.get("model"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let conversation_id = payload.get("conversation_id").cloned();

    let emit = |event: serde_json::Value| {
        host::call_host(
            host::HostFn::EmitProviderEvent,
            &serde_json::json!({
                "session_id": session_id,
                "event": event,
            }),
        )
    };

    if let Err(e) = emit(serde_json::json!({
        "kind": "started",
        "model": model,
        "conversation_id": conversation_id,
    })) {
        return cancel(&e);
    }
    if let Err(e) = emit(serde_json::json!({
        "kind": "text",
        "text": "not implemented",
    })) {
        return cancel(&e);
    }
    if let Err(e) = emit(serde_json::json!({
        "kind": "completed",
        "conversation_id": conversation_id,
    })) {
        return cancel(&e);
    }
    allow(serde_json::json!({ "ok": true }))
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
