//! User-defined MCP servers (Settings → MCP Servers).
//!
//! One JSON document (key [`MCP_SERVERS_KEY`]) in the core-settings
//! plugin-store collection holds every server the user configured. At
//! dispatch time — `SessionManager::send_message_locked`, the single
//! chokepoint where the model (hence provider) is finally resolved —
//! [`append_user_mcp_servers`] merges the provider-applicable entries into
//! the per-session `worker-mcp/{sid}.json` next to the built-in `peckboard`
//! entry. Claude consumes that file directly via `--mcp-config`; the Cursor
//! provider copies the extra entries into the workspace `.cursor/mcp.json`
//! (see `provider::cursor::mcp`); the Grok provider mirrors them into the
//! workspace `.mcp.json`, which Grok Build loads as a compatibility source
//! (see `provider::grok::mcp`); the Ollama provider connects a native MCP
//! client and folds the servers' tools into its inline tool loop (see
//! `service::mcp_client`). Only Mock has no external-MCP hook.
//!
//! Values (env vars, headers) are stored and returned verbatim — the editor
//! round-trips them, same trust model as a hand-written `.mcp.json`.

use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::routes::settings::{SETTINGS_COLLECTION, SETTINGS_NS};

/// Providers whose sessions can consume user-defined MCP servers.
pub const MCP_SUPPORTED_PROVIDERS: &[&str] = &["claude", "cursor", "grok", "ollama"];

/// Plugin-store key under [`SETTINGS_NS`]/[`SETTINGS_COLLECTION`].
pub const MCP_SERVERS_KEY: &str = "mcp_servers";

/// Name reserved for the built-in per-session server entry.
const RESERVED_NAME: &str = "peckboard";

/// One env-var or header row. A `Vec` (not a map) so the editor preserves
/// the user's row order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KvEntry {
    pub key: String,
    pub value: String,
}

/// One user-configured MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMcpServer {
    /// Stable identity for the editor (client-generated, e.g. a UUID).
    pub id: String,
    /// Key under `mcpServers` in generated configs.
    pub name: String,
    /// `stdio` | `http` | `sse`.
    pub transport: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<KvEntry>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub headers: Vec<KvEntry>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Provider ids this server applies to; empty = every supported provider.
    #[serde(default)]
    pub providers: Vec<String>,
}

fn default_true() -> bool {
    true
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Validate a full server list (the PUT body). Returns the first problem as
/// a user-facing message.
pub fn validate(servers: &[UserMcpServer]) -> Result<(), String> {
    let mut names = std::collections::HashSet::new();
    let mut ids = std::collections::HashSet::new();
    for s in servers {
        if s.id.trim().is_empty() {
            return Err("every server needs a non-empty id".into());
        }
        if !ids.insert(s.id.clone()) {
            return Err(format!("duplicate server id '{}'", s.id));
        }
        if !valid_name(&s.name) {
            return Err(format!(
                "server name '{}' must be 1-64 characters: letters, digits, '-' or '_'",
                s.name
            ));
        }
        if s.name.eq_ignore_ascii_case(RESERVED_NAME) {
            return Err("the name 'peckboard' is reserved for the built-in server".into());
        }
        if !names.insert(s.name.to_ascii_lowercase()) {
            return Err(format!("duplicate server name '{}'", s.name));
        }
        match s.transport.as_str() {
            "stdio" => {
                if s.command.trim().is_empty() {
                    return Err(format!(
                        "server '{}': a command is required for stdio transport",
                        s.name
                    ));
                }
            }
            "http" | "sse" => {
                if !(s.url.starts_with("http://") || s.url.starts_with("https://")) {
                    return Err(format!(
                        "server '{}': URL must start with http:// or https://",
                        s.name
                    ));
                }
            }
            other => {
                return Err(format!("server '{}': unknown transport '{other}'", s.name));
            }
        }
        for p in &s.providers {
            if !MCP_SUPPORTED_PROVIDERS.contains(&p.as_str()) {
                return Err(format!(
                    "server '{}': provider '{p}' does not support external MCP servers",
                    s.name
                ));
            }
        }
        for kv in s.env.iter().chain(s.headers.iter()) {
            if kv.key.trim().is_empty() {
                return Err(format!(
                    "server '{}': env/header rows need a non-empty key",
                    s.name
                ));
            }
        }
    }
    Ok(())
}

/// Render one server into the `mcpServers` entry shape the CLIs expect.
pub fn entry_json(s: &UserMcpServer) -> serde_json::Value {
    fn kv_map(rows: &[KvEntry]) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        for kv in rows {
            map.insert(kv.key.clone(), serde_json::Value::String(kv.value.clone()));
        }
        serde_json::Value::Object(map)
    }
    match s.transport.as_str() {
        "stdio" => {
            let mut entry = serde_json::json!({
                "type": "stdio",
                "command": s.command,
            });
            if !s.args.is_empty() {
                entry["args"] = serde_json::json!(s.args);
            }
            if !s.env.is_empty() {
                entry["env"] = kv_map(&s.env);
            }
            entry
        }
        t => {
            let mut entry = serde_json::json!({
                "type": t,
                "url": s.url,
            });
            if !s.headers.is_empty() {
                entry["headers"] = kv_map(&s.headers);
            }
            entry
        }
    }
}

/// Tolerant parse of the stored document — a broken document behaves like
/// an empty list rather than blocking dispatch.
pub fn parse_servers(raw: &str) -> Vec<UserMcpServer> {
    match serde_json::from_str::<Vec<UserMcpServer>>(raw) {
        Ok(list) => list,
        Err(e) => {
            tracing::warn!("stored mcp_servers document is invalid, ignoring: {e}");
            Vec::new()
        }
    }
}

/// Load the configured servers (empty when unset or unreadable).
pub async fn load(db: &Db) -> Vec<UserMcpServer> {
    let db = db.clone();
    tokio::task::spawn_blocking(move || {
        db.plugin_store_get_blocking(SETTINGS_NS, SETTINGS_COLLECTION, MCP_SERVERS_KEY)
    })
    .await
    .ok()
    .and_then(|r| r.ok())
    .flatten()
    .map(|raw| parse_servers(&raw))
    .unwrap_or_default()
}

fn applies_to(s: &UserMcpServer, provider_id: &str) -> bool {
    s.enabled && (s.providers.is_empty() || s.providers.iter().any(|p| p == provider_id))
}

/// The `(name, entry)` pairs to inject for `provider_id`.
pub fn entries_for_provider(
    servers: &[UserMcpServer],
    provider_id: &str,
) -> Vec<(String, serde_json::Value)> {
    servers
        .iter()
        .filter(|s| applies_to(s, provider_id))
        .map(|s| (s.name.clone(), entry_json(s)))
        .collect()
}

/// Non-peckboard `mcpServers` entries from a per-session worker-mcp config
/// file (read AFTER the dispatch-time merge, so already provider-filtered).
/// Empty on any read or shape problem — consumers run without extras rather
/// than fail the turn. Shared by the Grok (workspace mirror) and Ollama
/// (native client) providers.
pub fn extra_entries_from_session_config(path: &str) -> Vec<(String, serde_json::Value)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    json.get("mcpServers")
        .and_then(|v| v.as_object())
        .map(|map| {
            map.iter()
                .filter(|(name, _)| name.as_str() != RESERVED_NAME)
                .map(|(name, entry)| (name.clone(), entry.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Merge the user's provider-applicable servers into an already-written
/// per-session config file. Called once per dispatch from
/// `SessionManager::send_message_locked`; a fresh file is written before
/// every turn, so this never sees its own output. Best-effort: on any
/// failure the turn proceeds with just the built-in `peckboard` entry.
pub async fn append_user_mcp_servers(mcp_config_path: &str, db: &Db, provider_id: &str) {
    if !MCP_SUPPORTED_PROVIDERS.contains(&provider_id) {
        return;
    }
    let entries = entries_for_provider(&load(db).await, provider_id);
    if entries.is_empty() {
        return;
    }
    if let Err(e) = merge_into_config_file(mcp_config_path, &entries) {
        tracing::warn!("user MCP servers not injected into {mcp_config_path}: {e}");
    }
}

fn merge_into_config_file(
    path: &str,
    entries: &[(String, serde_json::Value)],
) -> anyhow::Result<()> {
    let mut root: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let servers = root
        .get_mut("mcpServers")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| anyhow::anyhow!("mcpServers is not an object"))?;
    for (name, entry) in entries {
        if name == RESERVED_NAME {
            continue;
        }
        servers.insert(name.clone(), entry.clone());
    }
    std::fs::write(path, serde_json::to_string_pretty(&root)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio_server(name: &str) -> UserMcpServer {
        UserMcpServer {
            id: format!("id-{name}"),
            name: name.into(),
            transport: "stdio".into(),
            command: "npx".into(),
            args: vec!["-y".into(), "@modelcontextprotocol/server-github".into()],
            env: vec![KvEntry {
                key: "GITHUB_TOKEN".into(),
                value: "t0k".into(),
            }],
            url: String::new(),
            headers: Vec::new(),
            enabled: true,
            providers: Vec::new(),
        }
    }

    fn http_server(name: &str) -> UserMcpServer {
        UserMcpServer {
            id: format!("id-{name}"),
            name: name.into(),
            transport: "http".into(),
            command: String::new(),
            args: Vec::new(),
            env: Vec::new(),
            url: "https://example.com/mcp".into(),
            headers: vec![KvEntry {
                key: "Authorization".into(),
                value: "Bearer x".into(),
            }],
            enabled: true,
            providers: Vec::new(),
        }
    }

    #[test]
    fn validate_accepts_good_list() {
        assert!(validate(&[stdio_server("github"), http_server("linear")]).is_ok());
    }

    #[test]
    fn validate_rejects_reserved_and_duplicate_names() {
        let err = validate(&[stdio_server("PeckBoard")]).unwrap_err();
        assert!(err.contains("reserved"));
        let err = validate(&[stdio_server("gh"), http_server("GH")]).unwrap_err();
        assert!(err.contains("duplicate server name"));
    }

    #[test]
    fn validate_rejects_bad_shapes() {
        let mut s = stdio_server("gh");
        s.command = "  ".into();
        assert!(validate(&[s]).unwrap_err().contains("command is required"));

        let mut s = http_server("web");
        s.url = "ftp://x".into();
        assert!(validate(&[s]).unwrap_err().contains("http://"));

        let mut s = stdio_server("gh");
        s.transport = "websocket".into();
        assert!(validate(&[s]).unwrap_err().contains("unknown transport"));

        let mut s = stdio_server("gh");
        s.name = "bad name!".into();
        assert!(validate(&[s]).unwrap_err().contains("1-64 characters"));

        let mut s = stdio_server("gh");
        s.providers = vec!["mock".into()];
        assert!(
            validate(&[s])
                .unwrap_err()
                .contains("does not support external MCP servers")
        );
    }

    #[test]
    fn entry_json_shapes() {
        let stdio = entry_json(&stdio_server("gh"));
        assert_eq!(stdio["type"], "stdio");
        assert_eq!(stdio["command"], "npx");
        assert_eq!(stdio["args"][1], "@modelcontextprotocol/server-github");
        assert_eq!(stdio["env"]["GITHUB_TOKEN"], "t0k");

        let http = entry_json(&http_server("linear"));
        assert_eq!(http["type"], "http");
        assert_eq!(http["url"], "https://example.com/mcp");
        assert_eq!(http["headers"]["Authorization"], "Bearer x");
        // No stdio keys leak into the http shape.
        assert!(http.get("command").is_none());
    }

    #[test]
    fn entries_filter_by_enabled_and_provider() {
        let mut off = stdio_server("off");
        off.enabled = false;
        let mut cursor_only = stdio_server("cursoronly");
        cursor_only.providers = vec!["cursor".into()];
        let everywhere = http_server("everywhere");

        let servers = vec![off, cursor_only, everywhere];
        let claude: Vec<_> = entries_for_provider(&servers, "claude")
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(claude, vec!["everywhere"]);
        let cursor: Vec<_> = entries_for_provider(&servers, "cursor")
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(cursor, vec!["cursoronly", "everywhere"]);
    }

    #[test]
    fn merge_appends_to_written_config_and_keeps_peckboard() {
        let tmp = tempfile::tempdir().unwrap();
        let path = super::super::config::write_mcp_config(tmp.path(), "sess-m", 4000, "tok")
            .unwrap()
            .to_string_lossy()
            .to_string();

        let entries = entries_for_provider(&[stdio_server("gh")], "claude");
        merge_into_config_file(&path, &entries).unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["mcpServers"]["peckboard"]["type"], "http");
        assert_eq!(json["mcpServers"]["gh"]["command"], "npx");

        // A hostile entry named "peckboard" is never merged over the built-in.
        let evil = vec![("peckboard".to_string(), serde_json::json!({"type":"stdio"}))];
        merge_into_config_file(&path, &evil).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["mcpServers"]["peckboard"]["type"], "http");
    }

    #[test]
    fn parse_is_tolerant() {
        assert!(parse_servers("not json").is_empty());
        assert!(parse_servers("[]").is_empty());
        let list = parse_servers(r#"[{"id":"a","name":"gh","transport":"stdio","command":"npx"}]"#);
        assert_eq!(list.len(), 1);
        assert!(list[0].enabled, "enabled defaults to true");
    }
}
