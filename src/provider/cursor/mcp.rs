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
//! Server approval (`cursor-agent mcp enable peckboard`) is sticky per
//! server config; it resets when the entry changes (e.g. a new port), so
//! [`approve_workspace_server`] runs before every turn — idempotent and
//! scoped to our server id, unlike `--approve-mcps` which would blanket-
//! approve any other MCP servers the user configured.

use std::path::Path;

/// Env var the workspace config references and each spawn populates.
pub const TOKEN_ENV_VAR: &str = "PECKBOARD_MCP_TOKEN";
/// Server id under `mcpServers`, matching `write_mcp_config`.
const SERVER_ID: &str = "peckboard";
/// Bound on the `mcp enable` subprocess.
const APPROVE_TIMEOUT_SECS: u64 = 10;

/// Endpoint + per-session bearer token for the peckboard MCP server.
pub struct McpWiring {
    pub url: String,
    pub token: String,
}

/// Extract url + bearer token from the per-session worker-mcp config JSON
/// written by `crate::service::mcp_server::write_mcp_config`. Returns `None`
/// (rather than erroring) on any shape mismatch — MCP is an optional
/// capability and the turn must still run without it.
pub fn parse_worker_mcp_config(path: &str) -> Option<McpWiring> {
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let server = json.get("mcpServers")?.get(SERVER_ID)?;
    let url = server.get("url")?.as_str()?.to_string();
    let auth = server.get("headers")?.get("Authorization")?.as_str()?;
    let token = auth.strip_prefix("Bearer ")?.to_string();
    Some(McpWiring { url, token })
}

/// Merge the peckboard server entry into `<working_dir>/.cursor/mcp.json`,
/// creating the file if needed and preserving any unrelated servers or
/// top-level keys. The written entry contains no secrets — its
/// Authorization header is the literal `Bearer ${env:PECKBOARD_MCP_TOKEN}`.
/// Returns `Ok(true)` when the file was (re)written, `Ok(false)` when it
/// already matched. A file that exists but is not valid JSON is left
/// untouched (error) rather than clobbered.
pub fn ensure_workspace_mcp_config(working_dir: &str, url: &str) -> anyhow::Result<bool> {
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

    let servers = root_obj
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("mcpServers in {} is not an object", path.display()))?;

    let desired = serde_json::json!({
        "type": "http",
        "url": url,
        "headers": { "Authorization": format!("Bearer ${{env:{TOKEN_ENV_VAR}}}") }
    });
    if servers.get(SERVER_ID) == Some(&desired) {
        return Ok(false);
    }
    servers.insert(SERVER_ID.to_string(), desired);

    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&root)?))?;
    Ok(true)
}

/// Approve the peckboard server for this workspace (`mcp enable` is also the
/// approval verb). Best-effort: failures are logged and the turn proceeds —
/// worst case the agent runs without peckboard tools, same as before wiring.
pub async fn approve_workspace_server(cli_path: &str, working_dir: &str, token: &str) {
    let status = tokio::time::timeout(
        std::time::Duration::from_secs(APPROVE_TIMEOUT_SECS),
        tokio::process::Command::new(cli_path)
            .args(["mcp", "enable", SERVER_ID])
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
        Ok(Ok(s)) => tracing::warn!("cursor: `mcp enable {SERVER_ID}` exited with {s}"),
        Ok(Err(e)) => tracing::warn!("cursor: could not run `mcp enable {SERVER_ID}`: {e}"),
        Err(_) => {
            tracing::warn!(
                "cursor: `mcp enable {SERVER_ID}` timed out after {APPROVE_TIMEOUT_SECS}s"
            )
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

    #[test]
    fn parses_the_config_write_mcp_config_produces() {
        let tmp = tempfile::tempdir().unwrap();
        let path =
            crate::service::mcp_server::write_mcp_config(tmp.path(), "sess-9", 4321, "tok-abc")
                .unwrap();
        let wiring = parse_worker_mcp_config(path.to_str().unwrap()).unwrap();
        assert_eq!(wiring.url, "http://127.0.0.1:4321/mcp");
        assert_eq!(wiring.token, "tok-abc");
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
        let changed = ensure_workspace_mcp_config(ws, "http://127.0.0.1:9000/mcp").unwrap();
        assert!(changed);
        let json = read_json(tmp.path());
        let server = &json["mcpServers"]["peckboard"];
        assert_eq!(server["type"], "http");
        assert_eq!(server["url"], "http://127.0.0.1:9000/mcp");
        assert_eq!(
            server["headers"]["Authorization"],
            "Bearer ${env:PECKBOARD_MCP_TOKEN}"
        );
    }

    #[test]
    fn ensure_is_idempotent_and_rewrites_stale_url() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        assert!(ensure_workspace_mcp_config(ws, "http://127.0.0.1:9000/mcp").unwrap());
        // Same url again: no rewrite.
        assert!(!ensure_workspace_mcp_config(ws, "http://127.0.0.1:9000/mcp").unwrap());
        // Port moved: entry rewritten.
        assert!(ensure_workspace_mcp_config(ws, "http://127.0.0.1:9001/mcp").unwrap());
        assert_eq!(
            read_json(tmp.path())["mcpServers"]["peckboard"]["url"],
            "http://127.0.0.1:9001/mcp"
        );
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
        assert!(ensure_workspace_mcp_config(ws, "http://127.0.0.1:9000/mcp").unwrap());
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
        assert!(ensure_workspace_mcp_config(ws, "http://127.0.0.1:9000/mcp").is_err());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(".cursor/mcp.json")).unwrap(),
            "{not json"
        );
    }
}
