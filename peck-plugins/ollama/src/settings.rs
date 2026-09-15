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

/// `(model without prefix/alias, base URL)` for `ollama:<model>` or `ollama:<model>@<server>`.
pub fn resolve_model_ref(raw: &str) -> (String, String) {
    let stripped = raw.strip_prefix("ollama:").unwrap_or(raw);
    match stripped.split_once('@') {
        Some((model, alias)) => {
            let url = servers()
                .into_iter()
                .find(|(n, _)| n == alias)
                .map(|(_, u)| u)
                .unwrap_or_else(base_url);
            (model.to_string(), url)
        }
        None => (stripped.to_string(), base_url()),
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
