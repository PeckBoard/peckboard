//! Per-session MCP config file.
//!
//! Agents connect to the in-process Rust MCP server (`src/routes/mcp.rs`,
//! `POST /mcp`) directly over its native HTTP transport — there is no
//! Node-based stdio bridge. The config just points the CLI at the loopback
//! `/mcp` endpoint and supplies the per-session bearer token as a header.

use std::path::{Path, PathBuf};

/// Write a per-session MCP config JSON file so workers can discover
/// the peckboard MCP endpoint.
pub fn write_mcp_config(
    data_dir: &Path,
    session_id: &str,
    http_port: u16,
    token: &str,
) -> anyhow::Result<PathBuf> {
    let mcp_dir = data_dir.join("worker-mcp");
    std::fs::create_dir_all(&mcp_dir)?;
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

    std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;
    Ok(config_path)
}

/// Remove a per-session MCP config file.
pub fn delete_mcp_config(data_dir: &Path, session_id: &str) {
    let config_path = data_dir
        .join("worker-mcp")
        .join(format!("{session_id}.json"));
    let _ = std::fs::remove_file(config_path);
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

        delete_mcp_config(tmp.path(), "sess-1");
        assert!(!path.exists());
    }

    #[test]
    fn test_delete_mcp_config_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        // Should not panic even if file doesn't exist
        delete_mcp_config(tmp.path(), "nonexistent");
    }
}
