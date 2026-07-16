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
//! The built-in `peckboard` server entry is deliberately NOT written for
//! grok — the grok provider runs its own tool loop and the peckboard tool
//! server isn't wired into it (see `send_message`). Only user-defined
//! servers (Settings → MCP Servers) ride along.

use std::path::Path;

/// Top-level key naming the entries Peckboard wrote (Grok Build only reads
/// `mcpServers` from this file; unknown top-level keys are ignored).
const MANAGED_KEY: &str = "peckboardManagedServers";
/// The built-in server id — never mirrored into grok's workspace file.
const RESERVED: &str = "peckboard";

/// Non-peckboard `mcpServers` entries from the per-session worker-mcp
/// config (already provider-filtered at dispatch time). Empty on any read
/// or shape problem — the turn must still run without MCP extras.
pub fn extra_servers_from_worker_config(path: &str) -> Vec<(String, serde_json::Value)> {
    crate::service::mcp_server::user_servers::extra_entries_from_session_config(path)
}

/// Mirror `extras` into `<working_dir>/.mcp.json`, preserving unrelated
/// servers and top-level keys. Previously-managed entries missing from
/// `extras` are removed. Returns `Ok(true)` when the file was (re)written.
/// A missing file with nothing to write stays missing; a file that exists
/// but is not valid JSON is left untouched (error) rather than clobbered.
pub fn ensure_workspace_mcp_json(
    working_dir: &str,
    extras: &[(String, serde_json::Value)],
) -> anyhow::Result<bool> {
    let path = Path::new(working_dir).join(".mcp.json");

    let existing = std::fs::read_to_string(&path).ok();
    if existing.is_none() && extras.is_empty() {
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
        assert!(!ensure_workspace_mcp_json(ws, &[]).unwrap());
        assert!(!tmp.path().join(".mcp.json").exists());
    }

    #[test]
    fn writes_tracks_and_removes_managed_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        let extras = vec![gh_entry()];

        assert!(ensure_workspace_mcp_json(ws, &extras).unwrap());
        let json = read_json(tmp.path());
        assert_eq!(json["mcpServers"]["github"]["command"], "npx");
        assert_eq!(json[MANAGED_KEY], serde_json::json!(["github"]));

        // Idempotent while unchanged.
        assert!(!ensure_workspace_mcp_json(ws, &extras).unwrap());

        // Deleted in Settings: entry and tracking key removed, file kept.
        assert!(ensure_workspace_mcp_json(ws, &[]).unwrap());
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
        assert!(ensure_workspace_mcp_json(ws, &extras).unwrap());
        assert!(ensure_workspace_mcp_json(ws, &[]).unwrap());
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
        assert!(ensure_workspace_mcp_json(ws, &[gh_entry()]).is_err());
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
        assert!(!ensure_workspace_mcp_json(ws2, &evil).unwrap());
        assert!(!tmp2.path().join(".mcp.json").exists());
    }
}
