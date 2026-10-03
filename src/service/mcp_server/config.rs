//! Per-session MCP config file.
//!
//! Agents connect to the in-process Rust MCP server (`src/routes/mcp.rs`,
//! `POST /mcp`) directly over its native HTTP transport — there is no
//! Node-based stdio bridge. The config just points the CLI at the loopback
//! `/mcp` endpoint and supplies the per-session bearer token as a header.
//!
//! The file holds a live bearer token, so it is written `0600` (atomically,
//! via a private temp file + rename), every leftover is swept at startup
//! (tokens never survive a restart), and the agent sandbox grants each agent
//! read access to its own file only.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Serialises writes and compare-and-delete of config files, so a turn
/// ending (which deletes the file only if it still holds that turn's token)
/// can't race a dispatch writing a fresh token for the next turn.
static CONFIG_LOCK: Mutex<()> = Mutex::new(());

fn config_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("worker-mcp")
}

/// Write a per-session MCP config JSON file so workers can discover
/// the peckboard MCP endpoint.
pub fn write_mcp_config(
    data_dir: &Path,
    session_id: &str,
    http_port: u16,
    token: &str,
) -> anyhow::Result<PathBuf> {
    let mcp_dir = config_dir(data_dir);
    std::fs::create_dir_all(&mcp_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&mcp_dir, std::fs::Permissions::from_mode(0o700));
    }
    let config_path = mcp_dir.join(format!("{session_id}.json"));

    // HTTP-transport MCP server: the CLI speaks JSON-RPC straight to the Rust
    // `/mcp` route. No `node` subprocess — the route now answers `initialize`
    // and `notifications/initialized` itself (previously faked by a proxy).
    let config = serde_json::json!({
        "mcpServers": {
            "peckboard": {
                "type": "http",
                "url": format!("http://127.0.0.1:{http_port}/mcp"),
                "headers": {
                    "Authorization": format!("Bearer {token}")
                }
            }
        }
    });
    let body = serde_json::to_string_pretty(&config)?;

    let _guard = CONFIG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = mcp_dir.join(format!(".{session_id}.{}.tmp", uuid::Uuid::new_v4()));
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(body.as_bytes())?;
    }
    if let Err(e) = std::fs::rename(&tmp, &config_path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(config_path)
}

/// The bearer token inside an MCP config file, if it parses.
pub fn read_mcp_config_token(path: &Path) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    v["mcpServers"]["peckboard"]["headers"]["Authorization"]
        .as_str()?
        .strip_prefix("Bearer ")
        .map(str::to_string)
}

/// Remove a per-session MCP config file.
pub fn delete_mcp_config(data_dir: &Path, session_id: &str) {
    let config_path = config_dir(data_dir).join(format!("{session_id}.json"));
    let _guard = CONFIG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _ = std::fs::remove_file(config_path);
}

/// Remove `path` only if it still carries `token` — i.e. no newer dispatch
/// has rewritten it for the next turn. Returns whether it was removed.
pub fn delete_mcp_config_if_token(path: &Path, token: &str) -> bool {
    let _guard = CONFIG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if read_mcp_config_token(path).as_deref() == Some(token) {
        return std::fs::remove_file(path).is_ok();
    }
    false
}

/// Startup sweep: every config left in `worker-mcp/` belongs to a previous
/// run whose in-memory tokens are gone, so all of it is dead weight (and a
/// pile of bearer tokens on disk). Returns how many files were removed.
pub fn sweep_mcp_configs(data_dir: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(config_dir(data_dir)) else {
        return 0;
    };
    let mut removed = 0;
    for e in rd.flatten() {
        if e.file_type().is_ok_and(|t| t.is_file()) && std::fs::remove_file(e.path()).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        tracing::info!("Removed {removed} stale MCP config file(s) from a previous run");
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_and_delete_mcp_config() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_mcp_config(tmp.path(), "sess-1", 3333, "tok123").unwrap();

        assert!(path.exists());
        let content: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // Config uses native HTTP transport (no node subprocess).
        assert_eq!(content["mcpServers"]["peckboard"]["type"], "http");
        assert_eq!(
            content["mcpServers"]["peckboard"]["url"],
            "http://127.0.0.1:3333/mcp"
        );
        assert_eq!(
            content["mcpServers"]["peckboard"]["headers"]["Authorization"],
            "Bearer tok123"
        );
        assert_eq!(read_mcp_config_token(&path).as_deref(), Some("tok123"));

        delete_mcp_config(tmp.path(), "sess-1");
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn config_is_private_and_swept() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = write_mcp_config(tmp.path(), "sess-1", 3333, "tok").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // Rewriting keeps a single file (no temp leftovers).
        write_mcp_config(tmp.path(), "sess-1", 3333, "tok2").unwrap();
        write_mcp_config(tmp.path(), "sess-2", 3333, "tok3").unwrap();
        assert_eq!(sweep_mcp_configs(tmp.path()), 2);
        assert_eq!(sweep_mcp_configs(tmp.path()), 0);
    }

    #[test]
    fn compare_and_delete_keeps_a_newer_token() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_mcp_config(tmp.path(), "s", 1, "old").unwrap();
        write_mcp_config(tmp.path(), "s", 1, "new").unwrap();
        assert!(!delete_mcp_config_if_token(&path, "old"));
        assert!(path.exists());
        assert!(delete_mcp_config_if_token(&path, "new"));
        assert!(!path.exists());
    }

    #[test]
    fn test_delete_mcp_config_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        // Should not panic even if file doesn't exist
        delete_mcp_config(tmp.path(), "nonexistent");
    }
}
