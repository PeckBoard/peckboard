//! Peckboard Grok CLI provider plugin (WASM / Extism).

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code, unused_imports))]

mod argv;
mod event;
mod host;
mod manifest;
mod mcp;
mod models;
mod parser;
mod send;
mod settings;

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
        "provider.models" => handle_models(),
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
        "capabilities": {
            "supports_thinking": true,
            "supports_images_in": false,
            "supports_usage": true,
            "supports_resume": true,
            "interrupt_kind": "hard_kill",
            "supports_mid_stream_injection": false,
            "answer_transport": "new_turn",
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
    match send::run(&payload) {
        Ok(()) => allow(serde_json::json!({ "ok": true })),
        Err(e) => cancel(&e),
    }
}

fn handle_models() -> String {
    let cli = settings::cli_path("grok");
    let extra = settings::str_list("additional_models");
    let mut discovered = Vec::new();
    if settings::bool("discover_models", true)
        && let Some(out) = settings::probe(&cli, &["models"])
        && let Some(cat) = parser::parse_cli_models(&out)
    {
        discovered = cat.models;
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
