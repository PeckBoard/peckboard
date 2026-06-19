//! Per-session MCP config file + stdio-to-HTTP proxy.

use std::path::{Path, PathBuf};

const PROXY_SCRIPT: &str = r#"#!/usr/bin/env node
import { createInterface } from 'readline';
import { request } from 'http';

const TOKEN = process.env.PECKBOARD_TOKEN;
const URL = process.env.PECKBOARD_MCP_URL;
const parsed = new globalThis.URL(URL);

const SERVER_INFO = {
  name: "peckboard",
  version: "1.0.0",
};
const CAPABILITIES = { tools: {} };

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + '\n');
}

function httpPost(body) {
  return new Promise((resolve, reject) => {
    const req = request({
      hostname: parsed.hostname,
      port: parsed.port,
      path: parsed.pathname,
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'Authorization': `Bearer ${TOKEN}`,
      },
    }, (res) => {
      let data = '';
      res.on('data', (c) => data += c);
      res.on('end', () => {
        try { resolve(JSON.parse(data)); }
        catch { resolve({ error: { code: -32000, message: data } }); }
      });
    });
    req.on('error', reject);
    req.write(body);
    req.end();
  });
}

const rl = createInterface({ input: process.stdin });
rl.on('line', async (line) => {
  if (!line.trim()) return;
  let msg;
  try { msg = JSON.parse(line); } catch { return; }

  // Handle MCP protocol messages locally
  if (msg.method === 'initialize') {
    send({
      jsonrpc: '2.0',
      id: msg.id,
      result: {
        protocolVersion: msg.params?.protocolVersion || '2024-11-05',
        serverInfo: SERVER_INFO,
        capabilities: CAPABILITIES,
      },
    });
    return;
  }

  if (msg.method === 'notifications/initialized') {
    // No response needed for notifications
    return;
  }

  // Forward everything else to the HTTP backend
  try {
    const result = await httpPost(line);
    send(result);
  } catch (e) {
    send({ jsonrpc: '2.0', id: msg.id, error: { code: -32000, message: String(e) } });
  }
});
"#;

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

    // Always rewrite the proxy script to pick up fixes.
    let proxy_path = mcp_dir.join("mcp-proxy.mjs");
    std::fs::write(&proxy_path, PROXY_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&proxy_path, std::fs::Permissions::from_mode(0o755))?;
    }

    let config = serde_json::json!({
        "mcpServers": {
            "peckboard": {
                "command": "node",
                "args": [proxy_path.to_string_lossy()],
                "env": {
                    "PECKBOARD_TOKEN": token,
                    "PECKBOARD_MCP_URL": format!("http://127.0.0.1:{http_port}/mcp")
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
        // Config uses command/args format (stdio subprocess)
        assert!(content["mcpServers"]["peckboard"]["command"].is_string());
        assert_eq!(
            content["mcpServers"]["peckboard"]["env"]["PECKBOARD_TOKEN"],
            "tok123"
        );
        assert_eq!(
            content["mcpServers"]["peckboard"]["env"]["PECKBOARD_MCP_URL"],
            "http://127.0.0.1:3333/mcp"
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
