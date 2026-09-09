//! Workspace MCP wiring for the Codex CLI.
//!
//! Codex loads MCP servers from `<workspace>/.codex/config.toml` under
//! `[mcp_servers.<name>]`. Streamable HTTP uses `url` +
//! `bearer_token_env_var` (the env *name*, never the secret). The real
//! token is injected per spawn as [`TOKEN_ENV_VAR`]. A managed block
//! (`BEGIN`/`END` markers) is rewritten each turn; TOML outside the
//! markers — including user-defined servers — is left untouched.

use std::path::Path;

const CODEX_TOML_BEGIN: &str = "# BEGIN PECKBOARD MCP";
const CODEX_TOML_END: &str = "# END PECKBOARD MCP";
const RESERVED: &str = "peckboard";
/// Env var the workspace config references; populated per spawn with the
/// session's MCP token. Never written into the file.
pub const TOKEN_ENV_VAR: &str = "PECKBOARD_MCP_TOKEN";

/// Non-peckboard `mcpServers` entries from the per-session worker-mcp
/// config (already provider-filtered at dispatch time).
pub fn extra_servers_from_worker_config(path: &str) -> Vec<(String, serde_json::Value)> {
    crate::service::mcp_server::user_servers::extra_entries_from_session_config(path)
}

/// Endpoint + per-session bearer token for the peckboard MCP server, plus
/// any user-defined server entries found alongside it.
pub struct McpWiring {
    pub url: String,
    pub token: String,
    pub extra_servers: Vec<(String, serde_json::Value)>,
}

/// Extract url + bearer token from the per-session worker-mcp config JSON
/// written by `crate::service::mcp_server::write_mcp_config`. Returns `None`
/// on any shape mismatch — MCP is optional and the turn must still run.
pub fn parse_worker_mcp_config(path: &str) -> Option<McpWiring> {
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
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

fn is_safe_server_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Merge the peckboard HTTP entry plus user-defined extras into
/// `<working_dir>/.codex/config.toml` as a managed block. Unrelated TOML
/// outside the markers is left untouched. Returns `Ok(true)` when the file
/// was (re)written.
pub fn ensure_workspace_codex_toml(
    working_dir: &str,
    peckboard_url: Option<&str>,
    extras: &[(String, serde_json::Value)],
) -> anyhow::Result<bool> {
    if extras.is_empty() && peckboard_url.is_none() {
        return Ok(false);
    }
    let dir = Path::new(working_dir).join(".codex");
    let path = dir.join("config.toml");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let without = strip_managed_toml_block(&existing);
    let block = render_managed_toml_block(peckboard_url, extras);
    if block.trim().is_empty() {
        return Ok(false);
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
    if next == existing {
        return Ok(false);
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, next)?;
    Ok(true)
}

fn strip_managed_toml_block(text: &str) -> String {
    let Some(start) = text.find(CODEX_TOML_BEGIN) else {
        return text.to_string();
    };
    let after_start = start + CODEX_TOML_BEGIN.len();
    let end = text[after_start..]
        .find(CODEX_TOML_END)
        .map(|i| after_start + i + CODEX_TOML_END.len())
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
             bearer_token_env_var = {token}\n\
             enabled = true\n",
            url = toml_string(url),
            token = toml_string(TOKEN_ENV_VAR),
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
    format!("{CODEX_TOML_BEGIN}\n{body}{CODEX_TOML_END}\n")
}

fn json_server_to_toml(name: &str, entry: &serde_json::Value) -> Option<String> {
    let fields = server_fields(entry);
    if fields.is_empty() {
        return None;
    }
    let mut lines = vec![format!("[mcp_servers.{name}]")];
    lines.extend(fields.iter().map(|(k, v)| format!("{k} = {v}")));
    lines.push("enabled = true".into());
    lines.push(String::new());
    Some(lines.join("\n"))
}

/// A worker `mcpServers` JSON entry as `(key, TOML value)` pairs, shared by the
/// managed-block renderer and the CLI `-c` overrides.
fn server_fields(entry: &serde_json::Value) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = Vec::new();
    if let Some(url) = entry.get("url").and_then(|v| v.as_str()) {
        fields.push(("url".into(), toml_string(url)));
        let bearer_var = bearer_env_from_headers(entry);
        if let Some(var) = &bearer_var {
            fields.push(("bearer_token_env_var".into(), toml_string(var)));
        }
        if let Some(headers) = entry.get("headers").and_then(|v| v.as_object()) {
            let mut literal: Vec<String> = Vec::new();
            let mut env_ref: Vec<String> = Vec::new();
            for (k, v) in headers {
                // Only an env-ref Authorization is consumed by bearer_token_env_var;
                // every other header still has to reach the server.
                if bearer_var.is_some() && k == "Authorization" {
                    continue;
                }
                let Some(val) = v.as_str() else { continue };
                match env_ref_name(val) {
                    Some(var) => env_ref.push(format!("{} = {}", toml_key(k), toml_string(&var))),
                    None => literal.push(format!("{} = {}", toml_key(k), toml_string(val))),
                }
            }
            if !literal.is_empty() {
                fields.push((
                    "http_headers".into(),
                    format!("{{ {} }}", literal.join(", ")),
                ));
            }
            if !env_ref.is_empty() {
                fields.push((
                    "env_http_headers".into(),
                    format!("{{ {} }}", env_ref.join(", ")),
                ));
            }
        }
    }
    if let Some(cmd) = entry.get("command").and_then(|v| v.as_str()) {
        fields.push(("command".into(), toml_string(cmd)));
    }
    if let Some(args) = entry.get("args").and_then(|v| v.as_array()) {
        let items: Vec<String> = args
            .iter()
            .filter_map(|v| v.as_str().map(toml_string))
            .collect();
        fields.push(("args".into(), format!("[{}]", items.join(", "))));
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
            fields.push(("env".into(), format!("{{ {} }}", items.join(", "))));
        }
    }
    fields
}

/// `key=value` pairs for `codex -c`, one per MCP server field.
///
/// Codex only layers a project's `.codex/config.toml` when the project is
/// *trusted*, so the managed block we write there is a no-op in a fresh
/// session folder (verified live with `codex mcp list --json`: untrusted
/// project → `[]`). CLI `-c` overrides have the highest precedence and skip
/// the trust gate, so they are the wiring we actually rely on.
pub fn cli_config_overrides(
    peckboard_url: Option<&str>,
    extras: &[(String, serde_json::Value)],
) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(url) = peckboard_url {
        out.push(format!("mcp_servers.{RESERVED}.url={}", toml_string(url)));
        out.push(format!(
            "mcp_servers.{RESERVED}.bearer_token_env_var={}",
            toml_string(TOKEN_ENV_VAR)
        ));
        out.push(format!("mcp_servers.{RESERVED}.enabled=true"));
    }
    for (name, entry) in extras {
        if name == RESERVED || !is_safe_server_name(name) {
            continue;
        }
        let fields = server_fields(entry);
        if fields.is_empty() {
            continue;
        }
        for (key, value) in fields {
            out.push(format!("mcp_servers.{name}.{key}={value}"));
        }
        out.push(format!("mcp_servers.{name}.enabled=true"));
    }
    out
}

/// `Authorization: Bearer ${VAR}` / `Bearer $VAR` → env var name. A literal
/// token is left for `http_headers` so we never invent an env binding.
fn bearer_env_from_headers(entry: &serde_json::Value) -> Option<String> {
    let auth = entry.get("headers")?.get("Authorization")?.as_str()?;
    env_ref_name(auth.strip_prefix("Bearer ")?)
}

/// `${VAR}` / `$VAR` → `VAR`. Anything else is a literal value.
fn env_ref_name(value: &str) -> Option<String> {
    let name = value
        .strip_prefix("${")
        .and_then(|s| s.strip_suffix('}'))
        .or_else(|| value.strip_prefix('$'))?;
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Some(name.to_string())
    } else {
        None
    }
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

    #[test]
    fn creates_peckboard_http_entry_with_bearer_env_var() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        assert!(ensure_workspace_codex_toml(ws, Some("http://127.0.0.1:4100/mcp"), &[]).unwrap());
        let text = std::fs::read_to_string(tmp.path().join(".codex/config.toml")).unwrap();
        assert!(text.contains("[mcp_servers.peckboard]"));
        assert!(text.contains("url = \"http://127.0.0.1:4100/mcp\""));
        assert!(text.contains("bearer_token_env_var = \"PECKBOARD_MCP_TOKEN\""));
        assert!(!text.contains("Bearer "));
    }

    #[test]
    fn ensure_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        assert!(
            ensure_workspace_codex_toml(ws, Some("http://127.0.0.1:4100/mcp"), &[gh_entry()])
                .unwrap()
        );
        assert!(
            !ensure_workspace_codex_toml(ws, Some("http://127.0.0.1:4100/mcp"), &[gh_entry()])
                .unwrap()
        );
    }

    #[test]
    fn does_not_clobber_user_servers_outside_the_managed_block() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        std::fs::create_dir_all(tmp.path().join(".codex")).unwrap();
        std::fs::write(
            tmp.path().join(".codex/config.toml"),
            "[mcp_servers.mine]\ncommand = \"keep-me\"\n\n[cli]\nfoo = 1\n",
        )
        .unwrap();

        assert!(
            ensure_workspace_codex_toml(ws, Some("http://127.0.0.1:4100/mcp"), &[gh_entry()])
                .unwrap()
        );
        let text = std::fs::read_to_string(tmp.path().join(".codex/config.toml")).unwrap();
        assert!(text.contains("[mcp_servers.mine]"));
        assert!(text.contains("command = \"keep-me\""));
        assert!(text.contains("[cli]"));
        assert!(text.contains("[mcp_servers.peckboard]"));
        assert!(text.contains("[mcp_servers.github]"));
        assert!(text.contains("command = \"npx\""));

        // Dropping extras rewrites only the managed block.
        assert!(ensure_workspace_codex_toml(ws, Some("http://127.0.0.1:4100/mcp"), &[]).unwrap());
        let text = std::fs::read_to_string(tmp.path().join(".codex/config.toml")).unwrap();
        assert!(text.contains("[mcp_servers.mine]"));
        assert!(text.contains("[mcp_servers.peckboard]"));
        assert!(!text.contains("[mcp_servers.github]"));
    }

    #[test]
    fn reserved_extra_never_shadows_peckboard() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_str().unwrap();
        let evil = vec![(
            "peckboard".to_string(),
            serde_json::json!({"type":"stdio","command":"evil"}),
        )];
        assert!(ensure_workspace_codex_toml(ws, Some("http://127.0.0.1:4100/mcp"), &evil).unwrap());
        let text = std::fs::read_to_string(tmp.path().join(".codex/config.toml")).unwrap();
        assert!(text.contains("bearer_token_env_var"));
        assert!(!text.contains("evil"));
    }
    #[test]
    fn cli_overrides_carry_the_wiring_past_the_project_trust_gate() {
        let overrides = cli_config_overrides(
            Some("http://127.0.0.1:4100/mcp"),
            &[
                gh_entry(),
                (
                    "peckboard".to_string(),
                    serde_json::json!({"command":"evil"}),
                ),
            ],
        );
        assert!(
            overrides.contains(&"mcp_servers.peckboard.url=\"http://127.0.0.1:4100/mcp\"".into())
        );
        assert!(overrides.contains(
            &"mcp_servers.peckboard.bearer_token_env_var=\"PECKBOARD_MCP_TOKEN\"".into()
        ));
        assert!(overrides.contains(&"mcp_servers.peckboard.enabled=true".into()));
        assert!(overrides.contains(&"mcp_servers.github.command=\"npx\"".into()));
        assert!(overrides.contains(&"mcp_servers.github.args=[\"-y\", \"gh-mcp\"]".into()));
        assert!(!overrides.iter().any(|o| o.contains("evil")));
    }

    #[test]
    fn env_ref_bearer_keeps_the_other_headers() {
        let entry = serde_json::json!({
            "type": "http",
            "url": "https://api.example.com/mcp",
            "headers": {
                "Authorization": "Bearer ${LINEAR_TOKEN}",
                "X-Tenant": "acme",
                "X-Trace": "${TRACE_ID}"
            }
        });
        let toml = json_server_to_toml("linear", &entry).unwrap();
        assert!(toml.contains("bearer_token_env_var = \"LINEAR_TOKEN\""));
        assert!(toml.contains("http_headers = { X-Tenant = \"acme\" }"));
        assert!(toml.contains("env_http_headers = { X-Trace = \"TRACE_ID\" }"));
        assert!(!toml.contains("Authorization"));
    }

    #[test]
    fn literal_bearer_stays_a_header_and_invents_no_env_binding() {
        let entry = serde_json::json!({
            "type": "http",
            "url": "https://api.example.com/mcp",
            "headers": {
                "Authorization": "Bearer sk-live-123",
                "X-Tenant": "acme"
            }
        });
        let toml = json_server_to_toml("linear", &entry).unwrap();
        assert!(!toml.contains("bearer_token_env_var"));
        assert!(toml.contains("Authorization = \"Bearer sk-live-123\""));
        assert!(toml.contains("X-Tenant = \"acme\""));
        assert!(!toml.contains(TOKEN_ENV_VAR));
    }

    #[test]
    fn parse_worker_mcp_config_reads_url_token_and_extras() {
        let tmp = tempfile::tempdir().unwrap();
        let path = crate::service::mcp_server::write_mcp_config(tmp.path(), "s-1", 4100, "tok")
            .unwrap()
            .to_string_lossy()
            .to_string();
        let mut json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        json["mcpServers"]["github"] = serde_json::json!({"type":"stdio","command":"npx"});
        std::fs::write(&path, json.to_string()).unwrap();

        let wiring = parse_worker_mcp_config(&path).expect("peckboard entry parses");
        assert!(wiring.url.contains("4100"));
        assert_eq!(wiring.token, "tok");
        assert_eq!(wiring.extra_servers.len(), 1);
        assert_eq!(wiring.extra_servers[0].0, "github");
        assert!(parse_worker_mcp_config("/nonexistent/x.json").is_none());
    }
}
