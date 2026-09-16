//! Workspace MCP wiring for the `grok` CLI, host-fs-free: the send loop
//! reads/writes the workspace files through the `provider_read_file` /
//! `provider_write_file` host fns, and this module only computes merged
//! contents.
//!
//! Grok loads MCP servers natively from `.grok/config.toml` and, as a
//! compatibility layer, from project `.mcp.json` files. Entries written to
//! `.mcp.json` are tracked under [`MANAGED_KEY`] so a server deleted in
//! Settings is removed on the next turn; hand-written entries and unrelated
//! top-level keys are never touched, and a file that isn't valid JSON is
//! refused rather than clobbered. The bearer token stays out of both files —
//! it is written as `Bearer ${PECKBOARD_MCP_TOKEN}` and the real value rides
//! [`TOKEN_ENV_VAR`] per spawn, so the files stay byte-identical across
//! concurrent sessions in one workspace.

pub const TOKEN_ENV_VAR: &str = "PECKBOARD_MCP_TOKEN";
const RESERVED: &str = "peckboard";
/// Top-level key naming the entries Peckboard wrote (grok only reads
/// `mcpServers` from `.mcp.json`; unknown top-level keys are ignored).
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

/// Non-peckboard `mcpServers` entries from the worker-mcp config. Used when
/// the config carries no peckboard entry — user-defined servers still ride
/// into the workspace files. Empty on any shape problem.
pub fn extra_servers_from_contents(contents: &str) -> Vec<(String, serde_json::Value)> {
    serde_json::from_str::<serde_json::Value>(contents)
        .ok()
        .and_then(|json| json.get("mcpServers")?.as_object().cloned())
        .map(|map| {
            map.iter()
                .filter(|(name, _)| name.as_str() != RESERVED)
                .map(|(name, entry)| (name.clone(), entry.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Server names grok accepts (letters, numbers, hyphens, underscores).
pub fn is_safe_server_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Merge the peckboard entry (when `peckboard_url` is set) plus the
/// user-defined `extras` into the existing `.mcp.json` contents, preserving
/// unrelated servers and top-level keys. Previously-managed entries missing
/// from `extras` are removed. Returns `Ok(Some(text))` when the file should
/// be (re)written, `Ok(None)` when nothing changed (or there is nothing at
/// all to write), and `Err` when the existing contents are not a JSON object
/// — which must be left untouched rather than clobbered.
pub fn merge_workspace_mcp_json(
    existing: Option<&str>,
    peckboard_url: Option<&str>,
    extras: &[(String, serde_json::Value)],
) -> Result<Option<String>, String> {
    if existing.is_none() && extras.is_empty() && peckboard_url.is_none() {
        return Ok(None);
    }

    let mut root: serde_json::Value = match existing {
        Some(text) => serde_json::from_str(text)
            .map_err(|e| format!(".mcp.json is not valid JSON ({e}); not touching it"))?,
        None => serde_json::json!({}),
    };
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| ".mcp.json is not a JSON object; not touching it".to_string())?;

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
        .ok_or_else(|| "mcpServers in .mcp.json is not an object".to_string())?;

    let mut changed = false;

    // The peckboard server itself: an HTTP entry whose bearer token is an
    // env reference, so the file carries no secret.
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
        return Ok(None);
    }
    serde_json::to_string_pretty(&root)
        .map(|s| Some(format!("{s}\n")))
        .map_err(|e| e.to_string())
}

const GROK_TOML_BEGIN: &str = "# BEGIN PECKBOARD MCP";
const GROK_TOML_END: &str = "# END PECKBOARD MCP";

/// Merge the peckboard HTTP entry plus user-defined extras into the existing
/// `.grok/config.toml` contents as a managed block. Grok always scans this
/// file; `.mcp.json` is Claude-compat and can be skipped after the import
/// prompt is dismissed. Unrelated TOML outside the managed markers is left
/// untouched. Returns `Some(text)` when the file should be (re)written,
/// `None` when nothing changed or there is nothing to write. `existing` is
/// `""` for a missing file.
pub fn merge_workspace_grok_toml(
    existing: &str,
    peckboard_url: Option<&str>,
    extras: &[(String, serde_json::Value)],
) -> Option<String> {
    if extras.is_empty() && peckboard_url.is_none() {
        return None;
    }
    let without = strip_managed_toml_block(existing);
    let block = render_managed_toml_block(peckboard_url, extras);
    if block.trim().is_empty() {
        return None;
    }
    let mut next = without.trim_end().to_string();
    if !next.is_empty() {
        next.push('\n');
        next.push('\n');
    }
    next.push_str(&block);
    if !next.ends_with('\n') {
        next.push('\n');
    }
    if next == existing { None } else { Some(next) }
}

fn strip_managed_toml_block(text: &str) -> String {
    let Some(start) = text.find(GROK_TOML_BEGIN) else {
        return text.to_string();
    };
    let after_start = start + GROK_TOML_BEGIN.len();
    let end = text[after_start..]
        .find(GROK_TOML_END)
        .map(|i| after_start + i + GROK_TOML_END.len())
        .unwrap_or(text.len());
    let mut out = String::new();
    out.push_str(text[..start].trim_end());
    let rest = text[end..].trim_start();
    if !out.is_empty() && !rest.is_empty() {
        out.push('\n');
        out.push('\n');
    }
    out.push_str(rest);
    out
}

fn render_managed_toml_block(
    peckboard_url: Option<&str>,
    extras: &[(String, serde_json::Value)],
) -> String {
    let mut body = String::new();
    if let Some(url) = peckboard_url {
        body.push_str(&format!(
            "[mcp_servers.{RESERVED}]\n\
             url = {url}\n\
             headers = {{ Authorization = \"Bearer ${{{TOKEN_ENV_VAR}}}\" }}\n\
             enabled = true\n",
            url = toml_string(url),
        ));
    }
    for (name, entry) in extras {
        if name == RESERVED || !is_safe_server_name(name) {
            continue;
        }
        if let Some(section) = json_server_to_toml(name, entry) {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&section);
        }
    }
    if body.is_empty() {
        return String::new();
    }
    format!("{GROK_TOML_BEGIN}\n{body}{GROK_TOML_END}\n")
}

fn json_server_to_toml(name: &str, entry: &serde_json::Value) -> Option<String> {
    let mut lines = vec![format!("[mcp_servers.{name}]")];
    if let Some(url) = entry.get("url").and_then(|v| v.as_str()) {
        lines.push(format!("url = {}", toml_string(url)));
    }
    if let Some(cmd) = entry.get("command").and_then(|v| v.as_str()) {
        lines.push(format!("command = {}", toml_string(cmd)));
    }
    if let Some(args) = entry.get("args").and_then(|v| v.as_array()) {
        let items: Vec<String> = args
            .iter()
            .filter_map(|v| v.as_str().map(toml_string))
            .collect();
        lines.push(format!("args = [{}]", items.join(", ")));
    }
    if let Some(headers) = entry.get("headers").and_then(|v| v.as_object())
        && !headers.is_empty()
    {
        let items: Vec<String> = headers
            .iter()
            .filter_map(|(k, v)| {
                let val = v.as_str()?;
                Some(format!("{} = {}", toml_key(k), toml_string(val)))
            })
            .collect();
        if !items.is_empty() {
            lines.push(format!("headers = {{ {} }}", items.join(", ")));
        }
    }
    if let Some(env) = entry.get("env").and_then(|v| v.as_object())
        && !env.is_empty()
    {
        let items: Vec<String> = env
            .iter()
            .filter_map(|(k, v)| {
                let val = v.as_str()?;
                Some(format!("{} = {}", toml_key(k), toml_string(val)))
            })
            .collect();
        if !items.is_empty() {
            lines.push(format!("env = {{ {} }}", items.join(", ")));
        }
    }
    if lines.len() == 1 {
        return None;
    }
    lines.push("enabled = true".into());
    lines.push(String::new());
    Some(lines.join("\n"))
}

fn toml_key(key: &str) -> String {
    if key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && !key.is_empty()
    {
        key.to_string()
    } else {
        toml_string(key)
    }
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gh_entry() -> (String, serde_json::Value) {
        (
            "github".to_string(),
            serde_json::json!({"type":"stdio","command":"npx","args":["-y","gh-mcp"]}),
        )
    }

    fn as_json(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn nothing_to_write_yields_none() {
        assert!(merge_workspace_mcp_json(None, None, &[]).unwrap().is_none());
    }

    #[test]
    fn writes_tracks_and_removes_managed_entries() {
        let extras = vec![gh_entry()];
        let first = merge_workspace_mcp_json(None, None, &extras)
            .unwrap()
            .expect("written");
        let json = as_json(&first);
        assert_eq!(json["mcpServers"]["github"]["command"], "npx");
        assert_eq!(json[MANAGED_KEY], serde_json::json!(["github"]));

        // Idempotent while unchanged.
        assert!(
            merge_workspace_mcp_json(Some(&first), None, &extras)
                .unwrap()
                .is_none()
        );

        // Deleted in Settings: entry and tracking key removed, file kept.
        let second = merge_workspace_mcp_json(Some(&first), None, &[])
            .unwrap()
            .expect("rewritten");
        let json = as_json(&second);
        assert!(json["mcpServers"].get("github").is_none());
        assert!(json.get(MANAGED_KEY).is_none());
    }

    #[test]
    fn preserves_hand_written_entries_and_keys() {
        let existing = r#"{"mcpServers":{"github":{"type":"http","url":"https://example.com/mcp"}},"custom":true}"#;
        let extras = vec![(
            "linear".to_string(),
            serde_json::json!({"type":"http","url":"https://linear.app/mcp"}),
        )];
        let first = merge_workspace_mcp_json(Some(existing), None, &extras)
            .unwrap()
            .expect("written");
        let second = merge_workspace_mcp_json(Some(&first), None, &[])
            .unwrap()
            .expect("rewritten");
        let json = as_json(&second);
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
        assert!(merge_workspace_mcp_json(Some("{not json"), None, &[gh_entry()]).is_err());

        let evil = vec![(
            "peckboard".to_string(),
            serde_json::json!({"type":"stdio","command":"evil"}),
        )];
        // Only a reserved entry → nothing to write.
        assert!(
            merge_workspace_mcp_json(None, None, &evil)
                .unwrap()
                .is_none()
        );
    }

    /// The peckboard entry is written with `"type":"http"` and an
    /// env-reference bearer token (grok expands `${VAR}` in `.mcp.json`
    /// headers), never the token itself; a same-named user entry can't
    /// shadow it.
    #[test]
    fn peckboard_entry_uses_an_env_reference_token() {
        let evil = vec![(
            "peckboard".to_string(),
            serde_json::json!({"type":"stdio","command":"evil"}),
        )];
        let text = merge_workspace_mcp_json(None, Some("http://127.0.0.1:4100/mcp"), &evil)
            .unwrap()
            .expect("written");
        let json = as_json(&text);
        let entry = &json["mcpServers"]["peckboard"];
        assert_eq!(entry["type"], "http");
        assert_eq!(entry["url"], "http://127.0.0.1:4100/mcp");
        assert_eq!(
            entry["headers"]["Authorization"],
            "Bearer ${PECKBOARD_MCP_TOKEN}"
        );
        assert!(entry.get("command").is_none(), "user entry must not shadow");
        // Not tracked as a managed user server, so it is never swept away.
        assert!(json.get(MANAGED_KEY).is_none());

        // Idempotent while unchanged.
        assert!(
            merge_workspace_mcp_json(Some(&text), Some("http://127.0.0.1:4100/mcp"), &[])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn extra_servers_surface_without_a_peckboard_entry() {
        let contents = r#"{"mcpServers":{"github":{"type":"stdio","command":"npx"}}}"#;
        assert!(parse_contents(contents).is_none());
        let extras = extra_servers_from_contents(contents);
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0].0, "github");
        assert!(extra_servers_from_contents("{not json").is_empty());
    }

    #[test]
    fn grok_toml_writes_peckboard_and_extras_as_a_managed_block() {
        let existing = "[cli]\ninstaller = \"internal\"\n";
        let text =
            merge_workspace_grok_toml(existing, Some("http://127.0.0.1:4100/mcp"), &[gh_entry()])
                .expect("written");
        assert!(text.contains("[cli]"));
        assert!(text.contains("installer = \"internal\""));
        assert!(text.contains("[mcp_servers.peckboard]"));
        assert!(text.contains("Bearer ${PECKBOARD_MCP_TOKEN}"));
        assert!(text.contains("[mcp_servers.github]"));
        assert!(text.contains("command = \"npx\""));

        // Idempotent while unchanged.
        assert!(
            merge_workspace_grok_toml(&text, Some("http://127.0.0.1:4100/mcp"), &[gh_entry()])
                .is_none()
        );

        // Replacing extras rewrites only the managed block.
        let next = merge_workspace_grok_toml(&text, Some("http://127.0.0.1:4100/mcp"), &[])
            .expect("rewritten");
        assert!(next.contains("[cli]"));
        assert!(next.contains("[mcp_servers.peckboard]"));
        assert!(!next.contains("[mcp_servers.github]"));
    }

    #[test]
    fn is_safe_server_name_matches_grok_rules() {
        assert!(is_safe_server_name("peckboard"));
        assert!(is_safe_server_name("github-bridge"));
        assert!(!is_safe_server_name("has space"));
        assert!(!is_safe_server_name("dot.name"));
        assert!(!is_safe_server_name(""));
    }
}
