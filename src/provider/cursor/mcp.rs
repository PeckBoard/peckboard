//! Workspace MCP wiring for `cursor-agent`.
//!
//! cursor-agent has no `--mcp-config` flag; it discovers MCP servers from
//! `.cursor/mcp.json` in the workspace and interpolates `${env:VAR}`
//! references inside server `headers` (both verified live, 2026-07-10 —
//! supersedes the NO-GO in the "cursor-agent CLI recon" report). That
//! combination gives per-session auth without per-session files: the
//! workspace file carries a static, secret-free entry referencing
//! [`TOKEN_ENV_VAR`], and the real bearer token is injected per spawn as an
//! environment variable. Concurrent sessions in the same workspace all want
//! identical file bytes, so there is nothing to collide on.
//!
//! User-defined MCP servers (Settings → MCP Servers) ride along: the
//! per-session worker-mcp file already carries the cursor-applicable
//! entries (merged at dispatch by `service::mcp_server::user_servers`), and
//! [`ensure_workspace_mcp_config`] copies them into the workspace file.
//! Entries written this way are tracked under the [`MANAGED_KEY`] top-level
//! list so a server deleted in Settings is removed from the workspace file
//! on the next turn — servers the user added to `.cursor/mcp.json` by hand
//! are never touched.
//!
//! Server approval (`cursor-agent mcp enable <name>`) is sticky per
//! server config; it resets when the entry changes (e.g. a new port), so
//! [`approve_workspace_server`] runs before every turn — idempotent and
//! scoped to the servers we wrote, unlike `--approve-mcps` which would
//! blanket-approve any other MCP servers the user configured.

use std::path::Path;

/// Env var the workspace config references and each spawn populates.
pub const TOKEN_ENV_VAR: &str = "PECKBOARD_MCP_TOKEN";
/// Server id under `mcpServers`, matching `write_mcp_config`.
const SERVER_ID: &str = "peckboard";
/// Top-level key in `.cursor/mcp.json` naming the user-defined servers
/// Peckboard wrote (so stale ones can be removed). cursor-agent ignores
/// unknown top-level keys.
const MANAGED_KEY: &str = "peckboardManagedServers";
/// Bound on the `mcp enable` subprocess.
const APPROVE_TIMEOUT_SECS: u64 = 10;

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
/// (rather than erroring) on any shape mismatch — MCP is an optional
/// capability and the turn must still run without it.
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

/// Merge the peckboard server entry plus the user-defined `extras` into
/// `<working_dir>/.cursor/mcp.json`, creating the file if needed and
/// preserving any unrelated servers or top-level keys. The peckboard entry
/// contains no secrets — its Authorization header is the literal
/// `Bearer ${env:PECKBOARD_MCP_TOKEN}`. Extras are written verbatim and
/// recorded under [`MANAGED_KEY`]; previously-managed entries missing from
/// `extras` are removed. Returns `Ok(true)` when the file was (re)written,
/// `Ok(false)` when it already matched. A file that exists but is not valid
/// JSON is left untouched (error) rather than clobbered.
pub fn ensure_workspace_mcp_config(
    working_dir: &str,
    url: &str,
    extras: &[(String, serde_json::Value)],
) -> anyhow::Result<bool> {
    let dir = Path::new(working_dir).join(".cursor");
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

    let desired = serde_json::json!({
        "type": "http",
        "url": url,
        "headers": { "Authorization": format!("Bearer ${{env:{TOKEN_ENV_VAR}}}") }
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

/// Approve one workspace server (`mcp enable` is also the approval verb).
/// Best-effort: failures are logged and the turn proceeds — worst case the
/// agent runs without that server's tools, same as before wiring.
pub async fn approve_workspace_server(
    cli_path: &str,
    working_dir: &str,
    token: &str,
    server_id: &str,
) {
    let status = tokio::time::timeout(
        std::time::Duration::from_secs(APPROVE_TIMEOUT_SECS),
        tokio::process::Command::new(cli_path)
            .args(["mcp", "enable", server_id])
            .current_dir(working_dir)
            // The approval is recorded against the env-INTERPOLATED server
            // config, so `enable` must resolve the exact same header value
            // as the turn — without this the hashes differ and the server
            // is silently dropped as unapproved.
            .env(TOKEN_ENV_VAR, token)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await;
    match status {
        Ok(Ok(s)) if s.success() => {}
        Ok(Ok(s)) => tracing::warn!("cursor: `mcp enable {server_id}` exited with {s}"),
        Ok(Err(e)) => tracing::warn!("cursor: could not run `mcp enable {server_id}`: {e}"),
        Err(_) => {
            tracing::warn!(
                "cursor: `mcp enable {server_id}` timed out after {APPROVE_TIMEOUT_SECS}s"
            )
        }
    }
}

/// Approve the peckboard server plus every managed user server for this
/// workspace, in sequence (each bounded by [`APPROVE_TIMEOUT_SECS`]).
pub async fn approve_workspace_servers(wiring: &McpWiring, cli_path: &str, working_dir: &str) {
    approve_workspace_server(cli_path, working_dir, &wiring.token, SERVER_ID).await;
    for (name, _) in &wiring.extra_servers {
        if name != SERVER_ID {
            approve_workspace_server(cli_path, working_dir, &wiring.token, name).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_json(dir: &Path) -> serde_json::Value {
        let text = std::fs::read_to_string(dir.join(".cursor/mcp.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn gh_entry() -> (String, serde_json::Value) {
        (
            "github".to_string(),
            serde_json::json!({"type":"stdio","command":"npx","args":["-y","gh-mcp"]}),
        )
    }

    #[test]
    fn parses_the_config_write_mcp_config_produces() {
        let tmp = tempfile::tempdir().unwrap();
        let path =
            crate::service::mcp_server::write_mcp_config(tmp.path(), "sess-9", 4321, "tok-abc")
                .unwrap();
        let wiring = parse_worker_mcp_config(path.to_str().unwrap()).unwrap();
        assert_eq!(wiring.url, "http://127.0.0.1:4321/mcp");
        assert_eq!(wiring.token, "tok-abc");
        assert!(wiring.extra_servers.is_empty());
    }

    #[test]
    fn parse_surfaces_user_entries_alongside_peckboard() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cfg.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "mcpServers": {
                    "peckboard": {
                        "type": "http",
                        "url": "http://127.0.0.1:4321/mcp",
                        "headers": {"Authorization": "Bearer tok"}
                    },
                    "github": {"type": "stdio", "command": "npx"}
                }
            })
            .to_string(),
        )
        .unwrap();
        let wiring = parse_worker_mcp_config(path.to_str().unwrap()).unwrap();
        assert_eq!(wiring.extra_servers.len(), 1);
        assert_eq!(wiring.extra_servers[0].0, "github");
        assert_eq!(wiring.extra_servers[0].1["command"], "npx");
    }

    #[test]
    fn parse_returns_none_for_missing_or_bad_file() {
        assert!(parse_worker_mcp_config("/nonexistent/x.json").is_none());
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("bad.json");
        std::fs::write(&bad, "{\"mcpServers\":{}}").unwrap();
        assert!(parse_worker_mcp_config(bad.to_str().unwrap()).is_none());
    }

    #[test]
    fn ensure_creates_secret_free_env_ref_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        let changed = ensure_workspace_mcp_config(ws, "http://127.0.0.1:9000/mcp", &[]).unwrap();
        assert!(changed);
        let json = read_json(tmp.path());
        let server = &json["mcpServers"]["peckboard"];
        assert_eq!(server["type"], "http");
        assert_eq!(server["url"], "http://127.0.0.1:9000/mcp");
        assert_eq!(
            server["headers"]["Authorization"],
            "Bearer ${env:PECKBOARD_MCP_TOKEN}"
        );
        // No extras → no managed-list key.
        assert!(json.get(MANAGED_KEY).is_none());
    }

    #[test]
    fn ensure_is_idempotent_and_rewrites_stale_url() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        assert!(ensure_workspace_mcp_config(ws, "http://127.0.0.1:9000/mcp", &[]).unwrap());
        // Same url again: no rewrite.
        assert!(!ensure_workspace_mcp_config(ws, "http://127.0.0.1:9000/mcp", &[]).unwrap());
        // Port moved: entry rewritten.
        assert!(ensure_workspace_mcp_config(ws, "http://127.0.0.1:9001/mcp", &[]).unwrap());
        assert_eq!(
            read_json(tmp.path())["mcpServers"]["peckboard"]["url"],
            "http://127.0.0.1:9001/mcp"
        );
    }

    #[test]
    fn ensure_writes_tracks_and_removes_managed_extras() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        let extras = vec![gh_entry()];

        assert!(ensure_workspace_mcp_config(ws, "http://127.0.0.1:9000/mcp", &extras).unwrap());
        let json = read_json(tmp.path());
        assert_eq!(json["mcpServers"]["github"]["command"], "npx");
        assert_eq!(json[MANAGED_KEY], serde_json::json!(["github"]));

        // Unchanged extras: idempotent.
        assert!(!ensure_workspace_mcp_config(ws, "http://127.0.0.1:9000/mcp", &extras).unwrap());

        // Server removed in Settings: entry and managed-list key go away.
        assert!(ensure_workspace_mcp_config(ws, "http://127.0.0.1:9000/mcp", &[]).unwrap());
        let json = read_json(tmp.path());
        assert!(json["mcpServers"].get("github").is_none());
        assert!(json.get(MANAGED_KEY).is_none());
        // The built-in entry survives removals.
        assert_eq!(json["mcpServers"]["peckboard"]["type"], "http");
    }

    #[test]
    fn ensure_never_removes_hand_written_servers() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        std::fs::create_dir_all(tmp.path().join(".cursor")).unwrap();
        std::fs::write(
            tmp.path().join(".cursor/mcp.json"),
            r#"{"mcpServers":{"github":{"type":"http","url":"https://example.com/mcp"}},"custom":true}"#,
        )
        .unwrap();
        // A managed extra under a DIFFERENT name comes and goes; the user's
        // hand-written "github" entry is never touched.
        let extras = vec![(
            "linear".to_string(),
            serde_json::json!({"type":"http","url":"https://linear.app/mcp"}),
        )];
        assert!(ensure_workspace_mcp_config(ws, "http://127.0.0.1:9000/mcp", &extras).unwrap());
        assert!(ensure_workspace_mcp_config(ws, "http://127.0.0.1:9000/mcp", &[]).unwrap());
        let json = read_json(tmp.path());
        assert_eq!(
            json["mcpServers"]["github"]["url"],
            "https://example.com/mcp"
        );
        assert!(json["mcpServers"].get("linear").is_none());
        assert_eq!(json["custom"], true);
    }

    #[test]
    fn ensure_preserves_foreign_servers_and_top_level_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        std::fs::create_dir_all(tmp.path().join(".cursor")).unwrap();
        std::fs::write(
            tmp.path().join(".cursor/mcp.json"),
            r#"{"mcpServers":{"github":{"type":"http","url":"https://example.com/mcp"}},"custom":true}"#,
        )
        .unwrap();
        assert!(ensure_workspace_mcp_config(ws, "http://127.0.0.1:9000/mcp", &[]).unwrap());
        let json = read_json(tmp.path());
        assert_eq!(
            json["mcpServers"]["github"]["url"],
            "https://example.com/mcp"
        );
        assert_eq!(json["custom"], true);
        assert_eq!(
            json["mcpServers"]["peckboard"]["url"],
            "http://127.0.0.1:9000/mcp"
        );
    }

    #[test]
    fn ensure_refuses_to_clobber_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        std::fs::create_dir_all(tmp.path().join(".cursor")).unwrap();
        std::fs::write(tmp.path().join(".cursor/mcp.json"), "{not json").unwrap();
        assert!(ensure_workspace_mcp_config(ws, "http://127.0.0.1:9000/mcp", &[]).is_err());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(".cursor/mcp.json")).unwrap(),
            "{not json"
        );
    }

    #[test]
    fn ensure_ignores_extra_named_peckboard() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        let extras = vec![(
            "peckboard".to_string(),
            serde_json::json!({"type":"stdio","command":"evil"}),
        )];
        assert!(ensure_workspace_mcp_config(ws, "http://127.0.0.1:9000/mcp", &extras).unwrap());
        let json = read_json(tmp.path());
        assert_eq!(json["mcpServers"]["peckboard"]["type"], "http");
        assert!(json.get(MANAGED_KEY).is_none());
    }
}
