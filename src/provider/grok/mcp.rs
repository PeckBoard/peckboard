//! Workspace MCP wiring for the `grok` CLI (Grok Build).
//!
//! Grok Build loads MCP servers natively from `.grok/config.toml` and, as
//! a compatibility layer, from project `.mcp.json` files (merged below
//! `config.toml` — see docs.x.ai/build/features/mcp-servers). Peckboard
//! uses the `.mcp.json` path: it is the exact `{"mcpServers": {...}}`
//! shape the per-session worker-mcp file already carries (no TOML
//! dependency, no format conversion), and JSON merging preserves any
//! hand-written entries. Entries written here are tracked under
//! [`MANAGED_KEY`] so a server deleted in Settings is removed from the
//! workspace file on the next turn; hand-written entries are never
//! touched, and the file is not created at all while there is nothing to
//! write.
//!
//! The built-in `peckboard` entry rides along too, so a grok session gets
//! the peckboard tools (card transitions, `run_command`, code tools) its own
//! tool loop otherwise has no route to. It is static and secret-free: the
//! bearer token is written as `Bearer ${PECKBOARD_MCP_TOKEN}` and the real
//! value is injected per spawn as [`TOKEN_ENV_VAR`] — verified against grok
//! 0.2.77, which expands `${VAR}` in `.mcp.json` header values (`grok mcp
//! doctor` against a local sniffer showed the resolved header). That keeps
//! the file identical for concurrent sessions in one workspace, the same
//! property kimi's `bearerTokenEnvVar` buys.

use std::path::Path;

/// Top-level key naming the entries Peckboard wrote (Grok Build only reads
/// `mcpServers` from this file; unknown top-level keys are ignored).
const MANAGED_KEY: &str = "peckboardManagedServers";
/// The built-in server id under `mcpServers`. A user-defined server of the
/// same name is ignored rather than allowed to shadow it.
const RESERVED: &str = "peckboard";
/// Env var the workspace `.mcp.json` references in its `Authorization`
/// header, populated per spawn with the session's MCP token.
pub const TOKEN_ENV_VAR: &str = "PECKBOARD_MCP_TOKEN";

/// Non-peckboard `mcpServers` entries from the per-session worker-mcp
/// config (already provider-filtered at dispatch time). Empty on any read
/// or shape problem — the turn must still run without MCP extras.
pub fn extra_servers_from_worker_config(path: &str) -> Vec<(String, serde_json::Value)> {
    crate::service::mcp_server::user_servers::extra_entries_from_session_config(path)
}
/// Endpoint + per-session bearer token for the peckboard MCP server, plus
/// any user-defined server entries found alongside it.
pub struct McpWiring {
    pub url: String,
    pub token: String,
    /// Non-peckboard `mcpServers` entries from the per-session file,
    /// verbatim — already provider-filtered at dispatch time.
    pub extra_servers: Vec<(String, serde_json::Value)>,
}

/// Extract url + bearer token from the per-session worker-mcp config JSON
/// written by `crate::service::mcp_server::write_mcp_config`. Returns `None`
/// (rather than erroring) on any shape mismatch — MCP is optional and the
/// turn must still run without it.
pub fn parse_worker_mcp_config(path: &str) -> Option<McpWiring> {
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
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

/// Merge the peckboard entry (when `peckboard_url` is set) plus the
/// user-defined `extras` into `<working_dir>/.mcp.json`, preserving
/// unrelated servers and top-level keys. Previously-managed entries missing
/// from `extras` are removed. Returns `Ok(true)` when the file was
/// (re)written. A missing file with nothing at all to write stays missing; a
/// file that exists but is not valid JSON is left untouched (error) rather
/// than clobbered.
pub fn ensure_workspace_mcp_json(
    working_dir: &str,
    peckboard_url: Option<&str>,
    extras: &[(String, serde_json::Value)],
) -> anyhow::Result<bool> {
    let path = Path::new(working_dir).join(".mcp.json");

    let existing = std::fs::read_to_string(&path).ok();
    if existing.is_none() && extras.is_empty() && peckboard_url.is_none() {
        return Ok(false);
    }

    let mut root: serde_json::Value = match &existing {
        Some(text) => serde_json::from_str(text).map_err(|e| {
            anyhow::anyhow!(
                "{} is not valid JSON ({e}); not touching it",
                path.display()
            )
        })?,
        None => serde_json::json!({}),
    };
    let root_obj = root.as_object_mut().ok_or_else(|| {
        anyhow::anyhow!("{} is not a JSON object; not touching it", path.display())
    })?;

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
        .ok_or_else(|| anyhow::anyhow!("mcpServers in {} is not an object", path.display()))?;

    let mut changed = false;

    // The peckboard server itself: an HTTP entry whose bearer token is an
    // env reference, so the file carries no secret and stays byte-identical
    // across concurrent sessions in the same workspace.
    if let Some(url) = peckboard_url {
        let desired = serde_json::json!({
            "type": "http",
            "url": url,
            "headers": { "Authorization": format!("Bearer ${{{TOKEN_ENV_VAR}}}") },
        });
        if servers.get(RESERVED) != Some(&desired) {
            servers.insert(RESERVED.to_string(), desired);
            changed = true;
        }
    }

    let managed_now: Vec<String> = extras
        .iter()
        .map(|(name, _)| name.clone())
        .filter(|name| name != RESERVED)
        .collect();
    for name in &previously_managed {
        if name != RESERVED && !managed_now.contains(name) && servers.remove(name).is_some() {
            changed = true;
        }
    }

    for (name, entry) in extras {
        if name == RESERVED {
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
        return Ok(false);
    }

    std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&root)?))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_json(dir: &Path) -> serde_json::Value {
        let text = std::fs::read_to_string(dir.join(".mcp.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn gh_entry() -> (String, serde_json::Value) {
        (
            "github".to_string(),
            serde_json::json!({"type":"stdio","command":"npx","args":["-y","gh-mcp"]}),
        )
    }

    #[test]
    fn extras_come_from_the_merged_worker_config() {
        let tmp = tempfile::tempdir().unwrap();
        let path = crate::service::mcp_server::write_mcp_config(tmp.path(), "s-1", 4100, "tok")
            .unwrap()
            .to_string_lossy()
            .to_string();
        // Freshly written config carries only the peckboard entry.
        assert!(extra_servers_from_worker_config(&path).is_empty());

        // After the dispatch-time merge, the user entries surface.
        let mut json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        json["mcpServers"]["github"] = serde_json::json!({"type":"stdio","command":"npx"});
        std::fs::write(&path, json.to_string()).unwrap();
        let extras = extra_servers_from_worker_config(&path);
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0].0, "github");

        assert!(extra_servers_from_worker_config("/nonexistent/x.json").is_empty());
    }

    #[test]
    fn nothing_to_write_creates_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        assert!(!ensure_workspace_mcp_json(ws, None, &[]).unwrap());
        assert!(!tmp.path().join(".mcp.json").exists());
    }

    #[test]
    fn writes_tracks_and_removes_managed_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        let extras = vec![gh_entry()];

        assert!(ensure_workspace_mcp_json(ws, None, &extras).unwrap());
        let json = read_json(tmp.path());
        assert_eq!(json["mcpServers"]["github"]["command"], "npx");
        assert_eq!(json[MANAGED_KEY], serde_json::json!(["github"]));

        // Idempotent while unchanged.
        assert!(!ensure_workspace_mcp_json(ws, None, &extras).unwrap());

        // Deleted in Settings: entry and tracking key removed, file kept.
        assert!(ensure_workspace_mcp_json(ws, None, &[]).unwrap());
        let json = read_json(tmp.path());
        assert!(json["mcpServers"].get("github").is_none());
        assert!(json.get(MANAGED_KEY).is_none());
    }

    #[test]
    fn preserves_hand_written_entries_and_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        std::fs::write(
            tmp.path().join(".mcp.json"),
            r#"{"mcpServers":{"github":{"type":"http","url":"https://example.com/mcp"}},"custom":true}"#,
        )
        .unwrap();
        let extras = vec![(
            "linear".to_string(),
            serde_json::json!({"type":"http","url":"https://linear.app/mcp"}),
        )];
        assert!(ensure_workspace_mcp_json(ws, None, &extras).unwrap());
        assert!(ensure_workspace_mcp_json(ws, None, &[]).unwrap());
        let json = read_json(tmp.path());
        // The user's own "github" entry (same name never managed) survives.
        assert_eq!(
            json["mcpServers"]["github"]["url"],
            "https://example.com/mcp"
        );
        assert!(json["mcpServers"].get("linear").is_none());
        assert_eq!(json["custom"], true);
    }

    #[test]
    fn refuses_to_clobber_invalid_json_and_skips_reserved() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        std::fs::write(tmp.path().join(".mcp.json"), "{not json").unwrap();
        assert!(ensure_workspace_mcp_json(ws, None, &[gh_entry()]).is_err());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap(),
            "{not json"
        );

        let tmp2 = tempfile::tempdir().unwrap();
        let ws2 = tmp2.path().to_str().unwrap();
        let evil = vec![(
            "peckboard".to_string(),
            serde_json::json!({"type":"stdio","command":"evil"}),
        )];
        // Only a reserved entry → nothing to write, no file created.
        assert!(!ensure_workspace_mcp_json(ws2, None, &evil).unwrap());
        assert!(!tmp2.path().join(".mcp.json").exists());
    }

    /// The peckboard entry is written with an env-reference bearer token
    /// (grok expands `${VAR}` in `.mcp.json` headers), never the token
    /// itself, and a same-named user entry can't shadow it.
    #[test]
    fn peckboard_entry_uses_an_env_reference_token() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        let evil = vec![(
            "peckboard".to_string(),
            serde_json::json!({"type":"stdio","command":"evil"}),
        )];

        assert!(ensure_workspace_mcp_json(ws, Some("http://127.0.0.1:4100/mcp"), &evil).unwrap());
        let json = read_json(tmp.path());
        let entry = &json["mcpServers"]["peckboard"];
        assert_eq!(entry["url"], "http://127.0.0.1:4100/mcp");
        assert_eq!(
            entry["headers"]["Authorization"],
            "Bearer ${PECKBOARD_MCP_TOKEN}"
        );
        assert!(entry.get("command").is_none(), "user entry must not shadow");
        // Not tracked as a managed user server, so it is never swept away.
        assert!(json.get(MANAGED_KEY).is_none());

        // Idempotent while unchanged.
        assert!(!ensure_workspace_mcp_json(ws, Some("http://127.0.0.1:4100/mcp"), &[]).unwrap());
    }

    #[test]
    fn parse_worker_mcp_config_reads_url_token_and_extras() {
        let tmp = tempfile::tempdir().unwrap();
        let path = crate::service::mcp_server::write_mcp_config(tmp.path(), "s-1", 4100, "tok")
            .unwrap()
            .to_string_lossy()
            .to_string();
        let mut json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        json["mcpServers"]["github"] = serde_json::json!({"type":"stdio","command":"npx"});
        std::fs::write(&path, json.to_string()).unwrap();

        let wiring = parse_worker_mcp_config(&path).expect("peckboard entry parses");
        assert!(wiring.url.contains("4100"));
        assert_eq!(wiring.token, "tok");
        assert_eq!(wiring.extra_servers.len(), 1);
        assert_eq!(wiring.extra_servers[0].0, "github");

        assert!(parse_worker_mcp_config("/nonexistent/x.json").is_none());
    }
}
