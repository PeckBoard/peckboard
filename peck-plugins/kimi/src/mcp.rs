//! Workspace MCP wiring for the `kimi` CLI (Kimi Code), host-fs-free: the
//! send loop reads/writes `.kimi-code/mcp.json` through the
//! `provider_read_file` / `provider_write_file` host fns, and this module
//! only computes merged contents.
//!
//! Kimi Code loads MCP servers from `mcp.json` at two levels — user
//! (`$KIMI_CODE_HOME/mcp.json`) and project (`.kimi-code/mcp.json` in the
//! working directory, which wins on name clashes) — and has no CLI flag for
//! it. Peckboard writes the PROJECT file. The `peckboard` entry is static
//! and secret-free: a `url` with no `transport` means HTTP, and the bearer
//! token is named via `bearerTokenEnvVar` and injected per spawn as
//! [`TOKEN_ENV_VAR`] — concurrent sessions in one workspace want identical
//! file bytes. User-defined servers ride along verbatim and are tracked
//! under [`MANAGED_KEY`] so a server deleted in Settings is removed on the
//! next turn; hand-written entries are never touched, and a file that isn't
//! valid JSON is refused rather than clobbered.

pub const TOKEN_ENV_VAR: &str = "PECKBOARD_MCP_TOKEN";
const RESERVED: &str = "peckboard";
/// Top-level key naming the entries Peckboard wrote (Kimi Code reads
/// `mcpServers` from this file; unknown top-level keys are ignored).
const MANAGED_KEY: &str = "peckboardManagedServers";

pub struct McpWiring {
    pub url: String,
    pub token: String,
    pub extra_servers: Vec<(String, serde_json::Value)>,
}

pub fn parse_contents(contents: &str) -> Option<McpWiring> {
    let json: serde_json::Value = serde_json::from_str(contents).ok()?;
    let servers = json.get("mcpServers")?;
    let server = servers.get(RESERVED)?;
    let url = server.get("url")?.as_str()?.to_string();
    let auth = server.get("headers")?.get("Authorization")?.as_str()?;
    let token = auth.strip_prefix("Bearer ")?.to_string();
    let extra_servers = servers
        .as_object()
        .map(|map| {
            map.iter()
                .filter(|(name, _)| name.as_str() != RESERVED)
                .map(|(name, entry)| (name.clone(), entry.clone()))
                .collect()
        })
        .unwrap_or_default();
    Some(McpWiring {
        url,
        token,
        extra_servers,
    })
}

pub fn is_safe_server_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Merge the peckboard entry plus the user-defined `extras` into the
/// existing `.kimi-code/mcp.json` contents, preserving unrelated servers and
/// top-level keys. Previously-managed extras missing from `extras` are
/// removed. Returns `Ok(Some(text))` when the file should be (re)written,
/// `Ok(None)` when it already matched, and `Err` when the existing contents
/// are not a JSON object — which must be left untouched rather than
/// clobbered.
pub fn merge_workspace_mcp_json(
    existing: Option<&str>,
    url: &str,
    extras: &[(String, serde_json::Value)],
) -> Result<Option<String>, String> {
    let mut root: serde_json::Value = match existing {
        Some(text) => serde_json::from_str(text)
            .map_err(|e| format!(".kimi-code/mcp.json is not valid JSON ({e}); not touching it"))?,
        None => serde_json::json!({}),
    };
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| ".kimi-code/mcp.json is not a JSON object; not touching it".to_string())?;

    let previously_managed: Vec<String> = root_obj
        .get(MANAGED_KEY)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let servers = root_obj
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| "mcpServers in .kimi-code/mcp.json is not an object".to_string())?;

    let mut changed = false;

    // A `url` with no `transport` is an HTTP server; the token stays out of
    // the file via `bearerTokenEnvVar` — kimi's documented mechanism (no
    // `${VAR}` header expansion like grok's).
    let desired = serde_json::json!({
        "url": url,
        "bearerTokenEnvVar": TOKEN_ENV_VAR,
    });
    if servers.get(RESERVED) != Some(&desired) {
        servers.insert(RESERVED.to_string(), desired);
        changed = true;
    }

    // Stale managed entries: written by a previous turn, no longer
    // configured (deleted or de-scoped in Settings).
    let managed_now: Vec<String> = extras
        .iter()
        .map(|(name, _)| name.clone())
        .filter(|name| name != RESERVED && is_safe_server_name(name))
        .collect();
    for name in &previously_managed {
        if name != RESERVED && !managed_now.contains(name) && servers.remove(name).is_some() {
            changed = true;
        }
    }

    for (name, entry) in extras {
        if name == RESERVED || !is_safe_server_name(name) {
            continue;
        }
        if servers.get(name) != Some(entry) {
            servers.insert(name.clone(), entry.clone());
            changed = true;
        }
    }

    if managed_now.is_empty() {
        if root_obj.remove(MANAGED_KEY).is_some() {
            changed = true;
        }
    } else {
        let managed_json = serde_json::json!(managed_now);
        if root_obj.get(MANAGED_KEY) != Some(&managed_json) {
            root_obj.insert(MANAGED_KEY.to_string(), managed_json);
            changed = true;
        }
    }

    if !changed {
        return Ok(None);
    }
    serde_json::to_string_pretty(&root)
        .map(|s| Some(format!("{s}\n")))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn as_json(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn writes_a_secret_free_peckboard_entry() {
        let text = merge_workspace_mcp_json(None, "http://127.0.0.1:4100/mcp", &[])
            .unwrap()
            .expect("written");
        let json = as_json(&text);
        assert_eq!(
            json["mcpServers"][RESERVED]["url"],
            "http://127.0.0.1:4100/mcp"
        );
        assert_eq!(
            json["mcpServers"][RESERVED]["bearerTokenEnvVar"],
            TOKEN_ENV_VAR
        );
        // No token bytes anywhere in the file — kimi uses the env-var name,
        // not a `Bearer ${VAR}` header (that's a grok-ism).
        assert!(!text.contains("Bearer "));
        assert!(json["mcpServers"][RESERVED].get("headers").is_none());

        // Idempotent while unchanged.
        assert!(
            merge_workspace_mcp_json(Some(&text), "http://127.0.0.1:4100/mcp", &[])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn tracks_and_removes_managed_extras() {
        let extras = vec![(
            "github".to_string(),
            serde_json::json!({"command":"npx","args":["-y","gh-mcp"]}),
        )];
        let first = merge_workspace_mcp_json(None, "http://x/mcp", &extras)
            .unwrap()
            .expect("written");
        let json = as_json(&first);
        assert_eq!(json["mcpServers"]["github"]["command"], "npx");
        assert_eq!(json[MANAGED_KEY], serde_json::json!(["github"]));

        // Deleted in Settings: entry and tracking key removed; the
        // peckboard entry stays.
        let second = merge_workspace_mcp_json(Some(&first), "http://x/mcp", &[])
            .unwrap()
            .expect("rewritten");
        let json = as_json(&second);
        assert!(json["mcpServers"].get("github").is_none());
        assert!(json.get(MANAGED_KEY).is_none());
        assert!(json["mcpServers"].get(RESERVED).is_some());
    }

    #[test]
    fn preserves_hand_written_entries_and_refuses_invalid_json() {
        let existing = r#"{"mcpServers":{"mine":{"url":"https://example.com/mcp"}},"custom":true}"#;
        let text = merge_workspace_mcp_json(Some(existing), "http://x/mcp", &[])
            .unwrap()
            .expect("written");
        let json = as_json(&text);
        assert_eq!(json["mcpServers"]["mine"]["url"], "https://example.com/mcp");
        assert_eq!(json["custom"], true);

        assert!(merge_workspace_mcp_json(Some("{not json"), "http://x/mcp", &[]).is_err());
    }

    #[test]
    fn reserved_or_unsafe_names_in_extras_never_land() {
        let evil = vec![
            (RESERVED.to_string(), serde_json::json!({"command":"evil"})),
            ("has space".to_string(), serde_json::json!({"command":"x"})),
        ];
        let text = merge_workspace_mcp_json(None, "http://x/mcp", &evil)
            .unwrap()
            .expect("written");
        let json = as_json(&text);
        assert_eq!(json["mcpServers"][RESERVED]["url"], "http://x/mcp");
        assert!(json["mcpServers"][RESERVED].get("command").is_none());
        assert!(json["mcpServers"].get("has space").is_none());
        assert!(json.get(MANAGED_KEY).is_none());
    }
}
