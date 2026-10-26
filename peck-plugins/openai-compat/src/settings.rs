//! Read this plugin's operator settings via the host.

use serde_json::{Value, json};

use crate::host::{self, HostFn};
use crate::models::PROVIDER_ID;

pub const DEFAULT_BASE_URL: &str = "http://localhost:8000";

pub fn value(key: &str) -> Option<Value> {
    let v = host::call_host(HostFn::GetPluginSetting, &json!({ "key": key })).ok()?;
    let val = v.get("value")?;
    if val.is_null() {
        None
    } else {
        Some(val.clone())
    }
}

pub fn str(key: &str) -> Option<String> {
    value(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub fn bool(key: &str, default: bool) -> bool {
    value(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

pub fn str_list(key: &str) -> Vec<String> {
    match value(key) {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                Vec::new()
            } else {
                vec![t.to_string()]
            }
        }
        _ => Vec::new(),
    }
}

pub fn kv_list(key: &str) -> Vec<(String, String)> {
    match value(key) {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| {
                let k = v.get("key")?.as_str()?.trim();
                let val = v.get("value")?.as_str()?.trim();
                if k.is_empty() || val.is_empty() {
                    None
                } else {
                    Some((k.to_string(), val.to_string()))
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub fn i64(key: &str, default: i64) -> i64 {
    value(key)
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap_or(default)
}

/// Seed + discovered + additional ids, de-duplicated in that order.
pub fn merge_catalog(
    seed: Value,
    discovered: Vec<String>,
    extra: Vec<String>,
    display: impl Fn(&str) -> String,
) -> Value {
    let mut models: Vec<Value> = seed.as_array().cloned().unwrap_or_default();
    let mut seen: Vec<String> = models
        .iter()
        .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    for id in discovered.into_iter().chain(extra) {
        if seen.iter().any(|s| s == &id) {
            continue;
        }
        seen.push(id.clone());
        models.push(json!({
            "id": id,
            "display_name": display(&id),
            "capabilities": ["code"],
            "tier": 0,
        }));
    }
    Value::Array(models)
}

/// Normalise a configured server URL to its root, so `{root}/v1/...` never
/// doubles the version segment: trailing slashes and a trailing `/v1` are
/// both dropped (`http://h:8000/v1/` → `http://h:8000`).
pub fn normalize_base(raw: &str) -> String {
    let t = raw.trim().trim_end_matches('/');
    t.strip_suffix("/v1")
        .unwrap_or(t)
        .trim_end_matches('/')
        .to_string()
}

pub fn base_url() -> String {
    normalize_base(&str("base_url").unwrap_or_else(|| DEFAULT_BASE_URL.into()))
}

pub fn servers() -> Vec<(String, String)> {
    kv_list("servers")
        .into_iter()
        .map(|(k, v)| (k, normalize_base(&v)))
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .collect()
}

/// `(model without prefix/alias, server root URL)` for
/// `openai-compat:<model>` or `openai-compat:<model>@<server>`. Splits on
/// the LAST `@` so model ids that contain `@` still resolve. An alias that
/// isn't configured is an error — silently sending the request to the
/// default server would surface as a confusing model-not-found instead.
pub fn resolve_model_ref(raw: &str) -> Result<(String, String), String> {
    let prefix = format!("{PROVIDER_ID}:");
    let stripped = raw.strip_prefix(prefix.as_str()).unwrap_or(raw);
    match stripped.rsplit_once('@') {
        Some((model, alias)) if !model.is_empty() && !alias.is_empty() => servers()
            .into_iter()
            .find(|(n, _)| n == alias)
            .map(|(_, url)| (model.to_string(), url))
            .ok_or_else(|| {
                format!(
                    "openai-compat plugin: model '{stripped}' references server '{alias}', \
                     which is not configured under Additional Servers"
                )
            }),
        _ => Ok((stripped.to_string(), base_url())),
    }
}

/// Request headers: JSON content type, `Authorization: Bearer <api_key>`
/// when a key is set, then Additional HTTP Headers (which win on clashes).
pub fn http_headers() -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("Content-Type".into(), json!("application/json"));
    if let Some(key) = str("api_key") {
        m.insert("Authorization".into(), json!(format!("Bearer {key}")));
    }
    for (k, v) in kv_list("additional_headers") {
        if !k.is_empty() {
            m.retain(|existing, _| !existing.eq_ignore_ascii_case(&k));
            m.insert(k, json!(v));
        }
    }
    m
}

pub fn timeout_secs() -> u64 {
    i64("request_timeout_secs", 600).clamp(1, 3600) as u64
}

/// Additional Models, plus the `models` list of the retired registry WASM
/// plugin that used this same plugin id — its saved settings carry over, so
/// an upgraded install keeps its models without re-entering them.
pub fn extra_models() -> Vec<String> {
    let mut ids = str_list("additional_models");
    for id in str_list("models") {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Host double answering only `peckboard_get_plugin_setting`, with one
    /// configured named server and an API key.
    fn settings_host() -> peck_plugin_native_host::HostFn {
        Arc::new(|name: &str, input: &str| {
            let input: Value = serde_json::from_str(input).unwrap_or(json!({}));
            if name != "peckboard_get_plugin_setting" {
                return json!({ "error": format!("unexpected host fn {name}") }).to_string();
            }
            let value = match input.get("key").and_then(|k| k.as_str()) {
                Some("servers") => json!([{ "key": "lan", "value": "http://box:8000/v1/" }]),
                Some("api_key") => json!("sk-test"),
                _ => Value::Null,
            };
            json!({ "value": value }).to_string()
        })
    }

    #[test]
    fn resolves_known_alias_and_splits_on_last_at() {
        peck_plugin_native_host::with_host(settings_host(), || {
            assert_eq!(
                resolve_model_ref("openai-compat:Qwen/Qwen3-32B@lan").unwrap(),
                ("Qwen/Qwen3-32B".to_string(), "http://box:8000".to_string())
            );
            // LAST `@` wins: the model half keeps any earlier `@`.
            assert_eq!(resolve_model_ref("a@b@lan").unwrap().0, "a@b".to_string());
            // No alias → default base_url.
            assert_eq!(
                resolve_model_ref("openai-compat:Qwen/Qwen3-32B").unwrap(),
                (
                    "Qwen/Qwen3-32B".to_string(),
                    "http://localhost:8000".to_string()
                )
            );
        });
    }

    #[test]
    fn unknown_alias_is_a_hard_error_not_a_fallback() {
        peck_plugin_native_host::with_host(settings_host(), || {
            let err = resolve_model_ref("openai-compat:m@nope").unwrap_err();
            assert!(
                err.contains("references server 'nope', which is not configured"),
                "unexpected error: {err}"
            );
        });
    }

    #[test]
    fn base_url_with_or_without_v1_normalises_to_root() {
        assert_eq!(normalize_base("http://h:8000"), "http://h:8000");
        assert_eq!(normalize_base("http://h:8000/"), "http://h:8000");
        assert_eq!(normalize_base("http://h:8000/v1"), "http://h:8000");
        assert_eq!(normalize_base(" http://h:8000/v1/ "), "http://h:8000");
        assert_eq!(normalize_base("http://h/proxy/v1"), "http://h/proxy");
    }

    #[test]
    fn api_key_becomes_bearer_header() {
        peck_plugin_native_host::with_host(settings_host(), || {
            let h = http_headers();
            assert_eq!(h["Authorization"], "Bearer sk-test");
            assert_eq!(h["Content-Type"], "application/json");
        });
    }

    /// Settings saved by the retired registry WASM plugin (same id): a
    /// `base_url` ending in `/v1` and a `models` list keep working.
    #[test]
    fn legacy_wasm_plugin_settings_carry_over() {
        let host: peck_plugin_native_host::HostFn = Arc::new(|_name: &str, input: &str| {
            let input: Value = serde_json::from_str(input).unwrap_or(json!({}));
            let value = match input.get("key").and_then(|k| k.as_str()) {
                Some("base_url") => json!("https://openrouter.ai/api/v1"),
                Some("models") => json!(["old-a", "shared"]),
                Some("additional_models") => json!(["shared", "new-b"]),
                _ => Value::Null,
            };
            json!({ "value": value }).to_string()
        });
        peck_plugin_native_host::with_host(host, || {
            assert_eq!(base_url(), "https://openrouter.ai/api");
            assert_eq!(extra_models(), vec!["shared", "new-b", "old-a"]);
        });
    }
}
