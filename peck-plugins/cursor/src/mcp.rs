//! Workspace MCP wiring for `cursor-agent`.
//!
//! cursor-agent has no `--mcp-config` flag; it discovers MCP servers from
//! `.cursor/mcp.json` in the workspace and interpolates `${env:VAR}`
//! references inside server `headers` (verified live, 2026-07-10). That
//! combination gives per-session auth without per-session files: the
//! workspace file carries a static, secret-free entry referencing
//! [`TOKEN_ENV_VAR`], and the real bearer token is injected per spawn as an
//! environment variable.
//!
//! User-defined MCP servers ride along verbatim and are tracked under the
//! [`MANAGED_KEY`] top-level list so a server deleted in Settings is removed
//! from the workspace file on the next turn — servers the user added to
//! `.cursor/mcp.json` by hand are never touched. The merge happens on file
//! *contents* (the host does the actual read/write): an existing file that
//! isn't valid JSON is refused rather than clobbered, and a no-op merge
//! skips the write.
//!
//! Server approval (`cursor-agent mcp enable <name>`) is sticky per server
//! config; it resets when the entry changes (e.g. a new port), so the send
//! path re-asserts it before every turn — idempotent, cached by the host's
//! probe TTL, and scoped to the servers we wrote (unlike `--approve-mcps`,
//! which would blanket-approve unrelated user servers).

pub const TOKEN_ENV_VAR: &str = "PECKBOARD_MCP_TOKEN";
const RESERVED: &str = "peckboard";
/// Top-level key in `.cursor/mcp.json` naming the user-defined servers
/// Peckboard wrote (so stale ones can be removed). cursor-agent ignores
/// unknown top-level keys.
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

/// Servers to `cursor-agent mcp enable` for this turn: peckboard first,
/// then every managed user server.
pub fn approval_server_names(wiring: &McpWiring) -> Vec<String> {
    let mut names = vec![RESERVED.to_string()];
    for (name, _) in &wiring.extra_servers {
        if name != RESERVED {
            names.push(name.clone());
        }
    }
    names
}

/// Merge the peckboard server entry plus the user-defined extras into the
/// existing `.cursor/mcp.json` contents, preserving unrelated servers and
/// top-level keys. The peckboard entry contains no secrets — its
/// Authorization header is the literal `Bearer ${env:PECKBOARD_MCP_TOKEN}`.
/// Extras are written verbatim and recorded under [`MANAGED_KEY`];
/// previously-managed entries missing from the wiring are removed. Returns
/// `Ok(Some(text))` when the file should be (re)written, `Ok(None)` when it
/// already matched, and `Err` for existing contents that are not a JSON
/// object — which must be left untouched rather than clobbered.
pub fn merge_workspace_mcp_json(
    existing: Option<&str>,
    wiring: &McpWiring,
) -> Result<Option<String>, String> {
    let mut root: serde_json::Value = match existing {
        Some(text) => serde_json::from_str(text)
            .map_err(|e| format!(".cursor/mcp.json is not valid JSON ({e}); not touching it"))?,
        None => serde_json::json!({}),
    };
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| ".cursor/mcp.json is not a JSON object; not touching it".to_string())?;

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
        .ok_or_else(|| "mcpServers in .cursor/mcp.json is not an object".to_string())?;

    let mut changed = false;

    let desired = serde_json::json!({
        "type": "http",
        "url": wiring.url,
        "headers": { "Authorization": format!("Bearer ${{env:{TOKEN_ENV_VAR}}}") }
    });
    if servers.get(RESERVED) != Some(&desired) {
        servers.insert(RESERVED.to_string(), desired);
        changed = true;
    }

    // Stale managed entries: written by a previous turn, no longer
    // configured (deleted or de-scoped in Settings).
    let managed_now: Vec<String> = wiring
        .extra_servers
        .iter()
        .map(|(name, _)| name.clone())
        .filter(|name| name != RESERVED)
        .collect();
    for name in &previously_managed {
        if name != RESERVED && !managed_now.contains(name) && servers.remove(name).is_some() {
            changed = true;
        }
    }

    for (name, entry) in &wiring.extra_servers {
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
        return Ok(None);
    }
    serde_json::to_string_pretty(&root)
        .map(|s| Some(format!("{s}\n")))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wiring(extras: Vec<(String, serde_json::Value)>) -> McpWiring {
        McpWiring {
            url: "http://127.0.0.1:9000/mcp".into(),
            token: "tok".into(),
            extra_servers: extras,
        }
    }

    fn gh_entry() -> (String, serde_json::Value) {
        (
            "github".to_string(),
            serde_json::json!({"type":"stdio","command":"npx","args":["-y","gh-mcp"]}),
        )
    }

    fn parse(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn merge_creates_secret_free_env_ref_entry_with_http_type() {
        let text = merge_workspace_mcp_json(None, &wiring(vec![]))
            .unwrap()
            .expect("fresh file is written");
        let json = parse(&text);
        let server = &json["mcpServers"]["peckboard"];
        assert_eq!(server["type"], "http");
        assert_eq!(server["url"], "http://127.0.0.1:9000/mcp");
        assert_eq!(
            server["headers"]["Authorization"],
            "Bearer ${env:PECKBOARD_MCP_TOKEN}"
        );
        assert!(!text.contains("tok"));
        assert!(json.get(MANAGED_KEY).is_none());
    }

    #[test]
    fn merge_is_idempotent_and_rewrites_stale_url() {
        let text = merge_workspace_mcp_json(None, &wiring(vec![]))
            .unwrap()
            .unwrap();
        assert!(
            merge_workspace_mcp_json(Some(&text), &wiring(vec![]))
                .unwrap()
                .is_none(),
            "unchanged merge must skip the write"
        );
        let moved = McpWiring {
            url: "http://127.0.0.1:9001/mcp".into(),
            token: "tok".into(),
            extra_servers: vec![],
        };
        let text2 = merge_workspace_mcp_json(Some(&text), &moved)
            .unwrap()
            .expect("moved port rewrites");
        assert_eq!(
            parse(&text2)["mcpServers"]["peckboard"]["url"],
            "http://127.0.0.1:9001/mcp"
        );
    }

    #[test]
    fn merge_tracks_and_removes_managed_extras() {
        let text = merge_workspace_mcp_json(None, &wiring(vec![gh_entry()]))
            .unwrap()
            .unwrap();
        let json = parse(&text);
        assert_eq!(json["mcpServers"]["github"]["command"], "npx");
        assert_eq!(json[MANAGED_KEY], serde_json::json!(["github"]));

        // Server removed in Settings: entry and managed-list key go away.
        let text2 = merge_workspace_mcp_json(Some(&text), &wiring(vec![]))
            .unwrap()
            .unwrap();
        let json = parse(&text2);
        assert!(json["mcpServers"].get("github").is_none());
        assert!(json.get(MANAGED_KEY).is_none());
        assert_eq!(json["mcpServers"]["peckboard"]["type"], "http");
    }

    #[test]
    fn merge_never_removes_hand_written_servers_or_foreign_keys() {
        let existing = r#"{"mcpServers":{"github":{"type":"http","url":"https://example.com/mcp"}},"custom":true}"#;
        // A managed extra under a DIFFERENT name comes and goes; the user's
        // hand-written "github" entry is never touched.
        let extras = vec![(
            "linear".to_string(),
            serde_json::json!({"type":"http","url":"https://linear.app/mcp"}),
        )];
        let text = merge_workspace_mcp_json(Some(existing), &wiring(extras))
            .unwrap()
            .unwrap();
        let text2 = merge_workspace_mcp_json(Some(&text), &wiring(vec![]))
            .unwrap()
            .unwrap();
        let json = parse(&text2);
        assert_eq!(
            json["mcpServers"]["github"]["url"],
            "https://example.com/mcp"
        );
        assert!(json["mcpServers"].get("linear").is_none());
        assert_eq!(json["custom"], true);
    }

    #[test]
    fn merge_refuses_invalid_json() {
        assert!(merge_workspace_mcp_json(Some("{not json"), &wiring(vec![])).is_err());
        assert!(merge_workspace_mcp_json(Some("[1,2]"), &wiring(vec![])).is_err());
    }

    #[test]
    fn merge_ignores_extra_named_peckboard() {
        let extras = vec![(
            "peckboard".to_string(),
            serde_json::json!({"type":"stdio","command":"evil"}),
        )];
        let text = merge_workspace_mcp_json(None, &wiring(extras))
            .unwrap()
            .unwrap();
        let json = parse(&text);
        assert_eq!(json["mcpServers"]["peckboard"]["type"], "http");
        assert!(!text.contains("evil"));
        assert!(json.get(MANAGED_KEY).is_none());
    }

    #[test]
    fn approval_names_are_peckboard_then_managed_extras() {
        let names = approval_server_names(&wiring(vec![
            gh_entry(),
            ("peckboard".into(), serde_json::json!({"command":"evil"})),
        ]));
        assert_eq!(names, vec!["peckboard".to_string(), "github".to_string()]);
    }
}
