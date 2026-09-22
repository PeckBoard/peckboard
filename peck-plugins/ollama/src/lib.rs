//! Peckboard Ollama provider plugin (WASM / Extism).

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code, unused_imports))]

mod catalog_cache;
mod event;
mod host;
mod manifest;
mod models;
mod send;
mod settings;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::host::HostFn;

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
        "provider.send" => match send::run(&payload) {
            Ok(()) => allow(serde_json::json!({ "ok": true })),
            Err(e) => cancel(&e),
        },
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
        "pricing": models::seed_pricing(),
        "capabilities": {
            "supports_thinking": true,
            "supports_images_in": true,
            "supports_usage": true,
            "supports_resume": true,
            "interrupt_kind": "cooperative",
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

fn handle_models() -> String {
    let extra = settings::str_list("additional_models");
    let discovery_on = settings::bool("discover_models", true);
    let mut discovered = Vec::new();
    if discovery_on {
        let mut targets: Vec<(Option<String>, String)> = vec![(None, settings::base_url())];
        for (name, url) in settings::servers() {
            targets.push((Some(name), url));
        }
        for (alias, url) in targets {
            if let Some(ids) = fetch_server_models(&url) {
                for id in ids {
                    discovered.push(match &alias {
                        Some(name) => format!("{id}@{name}"),
                        None => id,
                    });
                }
            }
        }
    }
    // Live server tags are the truth; on a dead daemon prefer the last-good
    // discovered list before the compile-time suggestions.
    let discovered_ids: Vec<String> = if discovery_on {
        let discovered_models = if discovered.is_empty() {
            None
        } else {
            Some(
                discovered
                    .iter()
                    .map(|id| {
                        json!({
                            "id": id,
                            "display_name": format!("{id} (Ollama)"),
                            "capabilities": ["code"],
                            "tier": 0,
                        })
                    })
                    .collect(),
            )
        };
        catalog_cache::resolve_base(discovered_models, Vec::new())
            .0
            .iter()
            .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(str::to_string))
            .collect()
    } else {
        Vec::new()
    };
    let models = settings::merge_catalog(models::seed_models(), discovered_ids, extra, &[], |id| {
        format!("{id} (Ollama)")
    });
    allow(serde_json::json!({ "models": models }))
}

fn fetch_server_models(base: &str) -> Option<Vec<String>> {
    let resp = host::call_host(
        HostFn::HttpRequest,
        &json!({
            "url": format!("{base}/v1/models"),
            "method": "GET",
            "headers": settings::http_headers(),
            "timeout_secs": 15,
        }),
    )
    .ok()?;
    let status = resp.get("status").and_then(|v| v.as_u64()).unwrap_or(0);
    if !(200..300).contains(&status) {
        return None;
    }
    let body = resp.get("body").and_then(|v| v.as_str())?;
    let parsed: Value = serde_json::from_str(body).ok()?;
    let data = parsed.get("data")?.as_array()?;
    Some(
        data.iter()
            .filter_map(|m| {
                m.get("id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            })
            .collect(),
    )
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
