//! Read this plugin's operator settings via the host.

use serde_json::{Value, json};

use crate::host::{self, HostFn};

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

pub fn cli_path(default: &str) -> String {
    str("cli_path").unwrap_or_else(|| default.to_string())
}

pub fn accounts() -> Vec<(String, String)> {
    let v = host::call_host(HostFn::ProviderListAccounts, &json!({})).ok();
    v.and_then(|v| v.get("accounts").and_then(|a| a.as_array()).cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|a| {
            Some((
                a.get("id")?.as_str()?.to_string(),
                a.get("name")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

pub fn probe(command: &str, args: &[&str]) -> Option<String> {
    let v = host::call_host(
        HostFn::ProviderProbe,
        &json!({
            "command": command,
            "args": args,
            "timeout_ms": 15_000,
        }),
    )
    .ok()?;
    v.get("stdout")?.as_str().map(str::to_string)
}

pub fn merge_catalog(
    seed: Value,
    discovered: Vec<String>,
    extra: Vec<String>,
    accounts: &[(String, String)],
    display: impl Fn(&str) -> String,
) -> Value {
    let mut models: Vec<Value> = seed.as_array().cloned().unwrap_or_default();
    let mut seen: Vec<String> = models
        .iter()
        .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    let mut add = |id: String| {
        if seen.iter().any(|s| s == &id) {
            return;
        }
        seen.push(id.clone());
        models.push(json!({
            "id": id,
            "display_name": display(&id),
            "capabilities": ["code"],
            "tier": 0,
        }));
    };
    for id in discovered.into_iter().chain(extra) {
        add(id);
    }
    if !accounts.is_empty() {
        let base = models.clone();
        for (acct_id, acct_name) in accounts {
            for m in &base {
                let Some(id) = m.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                if id.contains('@') {
                    continue;
                }
                let scoped = format!("{id}@{acct_id}");
                if seen.iter().any(|s| s == &scoped) {
                    continue;
                }
                seen.push(scoped.clone());
                let mut copy = m.clone();
                if let Some(obj) = copy.as_object_mut() {
                    obj.insert("id".into(), json!(scoped));
                    let name = obj
                        .get("display_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or(id);
                    obj.insert(
                        "display_name".into(),
                        json!(format!("[{acct_name}] {name}")),
                    );
                }
                models.push(copy);
            }
        }
    }
    Value::Array(models)
}

pub fn base_url() -> String {
    str("base_url")
        .unwrap_or_else(|| "http://localhost:11434".into())
        .trim_end_matches('/')
        .to_string()
}

pub fn servers() -> Vec<(String, String)> {
    kv_list("servers")
        .into_iter()
        .map(|(k, v)| (k, v.trim_end_matches('/').to_string()))
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .collect()
}

/// `(model without prefix/alias, base URL)` for `ollama:<model>` or
/// `ollama:<model>@<server>`. Splits on the LAST `@` so model names that
/// contain `@` still resolve. An alias that isn't configured is an error —
/// silently sending the request to the default server would surface as a
/// confusing model-not-found from Ollama instead.
pub fn resolve_model_ref(raw: &str) -> Result<(String, String), String> {
    let stripped = raw.strip_prefix("ollama:").unwrap_or(raw);
    match stripped.rsplit_once('@') {
        Some((model, alias)) if !model.is_empty() && !alias.is_empty() => servers()
            .into_iter()
            .find(|(n, _)| n == alias)
            .map(|(_, url)| (model.to_string(), url))
            .ok_or_else(|| {
                format!(
                    "ollama plugin: model '{stripped}' references server '{alias}', which \
                     is not configured under Additional Servers"
                )
            }),
        _ => Ok((stripped.to_string(), base_url())),
    }
}

pub fn http_headers() -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("Content-Type".into(), json!("application/json"));
    for (k, v) in kv_list("additional_headers") {
        if !k.is_empty() {
            m.insert(k, json!(v));
        }
    }
    m
}

pub fn timeout_secs() -> u64 {
    i64("request_timeout_secs", 600).clamp(1, 3600) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Host double answering only `peckboard_get_plugin_setting`, with one
    /// configured named server.
    fn settings_host() -> peck_plugin_native_host::HostFn {
        Arc::new(|name: &str, input: &str| {
            let input: Value = serde_json::from_str(input).unwrap_or(json!({}));
            if name != "peckboard_get_plugin_setting" {
                return json!({ "error": format!("unexpected host fn {name}") }).to_string();
            }
            let value = match input.get("key").and_then(|k| k.as_str()) {
                Some("servers") => json!([{ "key": "lan", "value": "http://box:11434/" }]),
                _ => Value::Null,
            };
            json!({ "value": value }).to_string()
        })
    }

    #[test]
    fn resolves_known_alias_and_splits_on_last_at() {
        peck_plugin_native_host::with_host(settings_host(), || {
            assert_eq!(
                resolve_model_ref("ollama:llama3.1@lan").unwrap(),
                ("llama3.1".to_string(), "http://box:11434".to_string())
            );
            // LAST `@` wins: the model half keeps any earlier `@`.
            assert_eq!(resolve_model_ref("a@b@lan").unwrap().0, "a@b".to_string());
            // No alias → default base_url.
            assert_eq!(
                resolve_model_ref("ollama:llama3.1").unwrap(),
                ("llama3.1".to_string(), "http://localhost:11434".to_string())
            );
        });
    }

    #[test]
    fn unknown_alias_is_a_hard_error_not_a_fallback() {
        peck_plugin_native_host::with_host(settings_host(), || {
            let err = resolve_model_ref("ollama:llama3.1@nope").unwrap_err();
            assert!(
                err.contains("references server 'nope', which is not configured"),
                "unexpected error: {err}"
            );
        });
    }
}
