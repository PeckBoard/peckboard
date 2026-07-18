//! Workspace MCP wiring for the `kimi` CLI (Kimi Code).
//!
//! Kimi Code loads MCP servers from `mcp.json` at two levels — user
//! (`$KIMI_CODE_HOME/mcp.json`) and project (`.kimi-code/mcp.json` in the
//! working directory, which wins on name clashes) — and has no CLI flag
//! for it (verified against kimi-code 0.27.0 `--help` and
//! moonshotai.github.io/kimi-code/en/customization/mcp). Peckboard writes
//! the PROJECT file: the user level lives inside each account's isolated
//! `KIMI_CODE_HOME`, while the project file follows the session's working
//! directory exactly like cursor's `.cursor/mcp.json`.
//!
//! The `peckboard` entry is static and secret-free: a `url` with no
//! `transport` means HTTP, and the bearer token is named via
//! `bearerTokenEnvVar` and injected per spawn as [`TOKEN_ENV_VAR`] —
//! concurrent sessions in one workspace want identical file bytes, so
//! there is nothing to collide on. User-defined servers (Settings → MCP
//! Servers) ride along verbatim from the per-session worker-mcp file and
//! are tracked under [`MANAGED_KEY`] so a server deleted in Settings is
//! removed on the next turn; hand-written entries are never touched.

use std::path::Path;

/// Env var the project mcp.json references (`bearerTokenEnvVar`) and each
/// spawn populates with the per-session MCP token.
pub const TOKEN_ENV_VAR: &str = "PECKBOARD_MCP_TOKEN";
/// Server id under `mcpServers`, matching `write_mcp_config`.
const SERVER_ID: &str = "peckboard";
/// Top-level key naming the user-defined entries Peckboard wrote (Kimi
/// Code reads `mcpServers` from this file; unknown top-level keys are
/// ignored).
const MANAGED_KEY: &str = "peckboardManagedServers";

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
/// written by `crate::service::mcp_server::write_mcp_config`. Returns
/// `None` (rather than erroring) on any shape mismatch — MCP is an
/// optional capability and the turn must still run without it.
pub fn parse_worker_mcp_config(path: &str) -> Option<McpWiring> {
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let servers = json.get("mcpServers")?;
    let server = servers.get(SERVER_ID)?;
    let url = server.get("url")?.as_str()?.to_string();
    let auth = server.get("headers")?.get("Authorization")?.as_str()?;
    let token = auth.strip_prefix("Bearer ")?.to_string();
    let extra_servers = servers
        .as_object()
        .map(|map| {
            map.iter()
                .filter(|(name, _)| name.as_str() != SERVER_ID)
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

/// Merge the peckboard entry plus the user-defined `extras` into
/// `<working_dir>/.kimi-code/mcp.json`, creating it if needed and
/// preserving unrelated servers and top-level keys. Previously-managed
/// extras missing from `extras` are removed. Returns `Ok(true)` when the
/// file was (re)written, `Ok(false)` when it already matched. A file that
/// exists but is not valid JSON is left untouched (error) rather than
/// clobbered.
pub fn ensure_workspace_mcp_config(
    working_dir: &str,
    url: &str,
    extras: &[(String, serde_json::Value)],
) -> anyhow::Result<bool> {
    let dir = Path::new(working_dir).join(".kimi-code");
    let path = dir.join("mcp.json");

    let mut root: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            anyhow::anyhow!(
                "{} is not valid JSON ({e}); not touching it",
                path.display()
            )
        })?,
        Err(_) => serde_json::json!({}),
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

    // A `url` with no `transport` is an HTTP server; the token stays out
    // of the file via `bearerTokenEnvVar`.
    let desired = serde_json::json!({
        "url": url,
        "bearerTokenEnvVar": TOKEN_ENV_VAR,
    });
    if servers.get(SERVER_ID) != Some(&desired) {
        servers.insert(SERVER_ID.to_string(), desired);
        changed = true;
    }

    // Stale managed entries: written by a previous turn, no longer
    // configured (deleted or de-scoped in Settings).
    let managed_now: Vec<String> = extras
        .iter()
        .map(|(name, _)| name.clone())
        .filter(|name| name != SERVER_ID)
        .collect();
    for name in &previously_managed {
        if name != SERVER_ID && !managed_now.contains(name) && servers.remove(name).is_some() {
            changed = true;
        }
    }

    for (name, entry) in extras {
        if name == SERVER_ID {
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
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&root)?))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_json(dir: &Path) -> serde_json::Value {
        let text = std::fs::read_to_string(dir.join(".kimi-code").join("mcp.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn parses_the_worker_config_written_by_the_server() {
        let tmp = tempfile::tempdir().unwrap();
        let path = crate::service::mcp_server::write_mcp_config(tmp.path(), "s-1", 4100, "tok-9")
            .unwrap()
            .to_string_lossy()
            .to_string();
        let wiring = parse_worker_mcp_config(&path).unwrap();
        assert_eq!(wiring.url, "http://127.0.0.1:4100/mcp");
        assert_eq!(wiring.token, "tok-9");
        assert!(wiring.extra_servers.is_empty());

        // User-defined servers merged at dispatch surface as extras.
        let mut json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        json["mcpServers"]["github"] = serde_json::json!({"url":"https://gh.example/mcp"});
        std::fs::write(&path, json.to_string()).unwrap();
        let wiring = parse_worker_mcp_config(&path).unwrap();
        assert_eq!(wiring.extra_servers.len(), 1);
        assert_eq!(wiring.extra_servers[0].0, "github");

        assert!(parse_worker_mcp_config("/nonexistent/x.json").is_none());
    }

    #[test]
    fn writes_a_secret_free_peckboard_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        assert!(ensure_workspace_mcp_config(ws, "http://127.0.0.1:4100/mcp", &[]).unwrap());
        let json = read_json(tmp.path());
        assert_eq!(
            json["mcpServers"][SERVER_ID]["url"],
            "http://127.0.0.1:4100/mcp"
        );
        assert_eq!(
            json["mcpServers"][SERVER_ID]["bearerTokenEnvVar"],
            TOKEN_ENV_VAR
        );
        // No token bytes anywhere in the file.
        let text = std::fs::read_to_string(tmp.path().join(".kimi-code/mcp.json")).unwrap();
        assert!(!text.contains("Bearer "));

        // Idempotent while unchanged.
        assert!(!ensure_workspace_mcp_config(ws, "http://127.0.0.1:4100/mcp", &[]).unwrap());
    }

    #[test]
    fn tracks_and_removes_managed_extras() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        let extras = vec![(
            "github".to_string(),
            serde_json::json!({"command":"npx","args":["-y","gh-mcp"]}),
        )];

        assert!(ensure_workspace_mcp_config(ws, "http://x/mcp", &extras).unwrap());
        let json = read_json(tmp.path());
        assert_eq!(json["mcpServers"]["github"]["command"], "npx");
        assert_eq!(json[MANAGED_KEY], serde_json::json!(["github"]));

        // Deleted in Settings: entry and tracking key removed; the
        // peckboard entry stays.
        assert!(ensure_workspace_mcp_config(ws, "http://x/mcp", &[]).unwrap());
        let json = read_json(tmp.path());
        assert!(json["mcpServers"].get("github").is_none());
        assert!(json.get(MANAGED_KEY).is_none());
        assert!(json["mcpServers"].get(SERVER_ID).is_some());
    }

    #[test]
    fn preserves_hand_written_entries_and_refuses_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        std::fs::create_dir_all(tmp.path().join(".kimi-code")).unwrap();
        std::fs::write(
            tmp.path().join(".kimi-code/mcp.json"),
            r#"{"mcpServers":{"mine":{"url":"https://example.com/mcp"}},"custom":true}"#,
        )
        .unwrap();
        assert!(ensure_workspace_mcp_config(ws, "http://x/mcp", &[]).unwrap());
        let json = read_json(tmp.path());
        assert_eq!(json["mcpServers"]["mine"]["url"], "https://example.com/mcp");
        assert_eq!(json["custom"], true);

        let tmp2 = tempfile::tempdir().unwrap();
        let ws2 = tmp2.path().to_str().unwrap();
        std::fs::create_dir_all(tmp2.path().join(".kimi-code")).unwrap();
        std::fs::write(tmp2.path().join(".kimi-code/mcp.json"), "{not json").unwrap();
        assert!(ensure_workspace_mcp_config(ws2, "http://x/mcp", &[]).is_err());
        assert_eq!(
            std::fs::read_to_string(tmp2.path().join(".kimi-code/mcp.json")).unwrap(),
            "{not json"
        );
    }

    #[test]
    fn reserved_name_in_extras_never_overrides_peckboard() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        let evil = vec![(SERVER_ID.to_string(), serde_json::json!({"command":"evil"}))];
        assert!(ensure_workspace_mcp_config(ws, "http://x/mcp", &evil).unwrap());
        let json = read_json(tmp.path());
        assert_eq!(json["mcpServers"][SERVER_ID]["url"], "http://x/mcp");
        assert!(json["mcpServers"][SERVER_ID].get("command").is_none());
        assert!(json.get(MANAGED_KEY).is_none());
    }
}
