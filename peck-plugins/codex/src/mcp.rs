//! MCP wiring for the Codex CLI, passed as `-c` config overrides.
//!
//! Codex only layers a project's `.codex/config.toml` when the project is
//! *trusted*, so a workspace file alone is a no-op in a fresh session
//! folder. CLI `-c` overrides have the highest precedence and skip the
//! trust gate, so they are the wiring we rely on. Streamable HTTP uses
//! `url` + `bearer_token_env_var` (the env *name*, never the secret); the
//! real token is injected per spawn as [`TOKEN_ENV_VAR`].

pub const TOKEN_ENV_VAR: &str = "PECKBOARD_MCP_TOKEN";
const RESERVED: &str = "peckboard";

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

fn is_safe_server_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
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

/// `Authorization: Bearer ${VAR}` / `Bearer $VAR` → env var name. A literal
/// token is left for `http_headers` so we never invent an env binding.
fn bearer_env_from_headers(entry: &serde_json::Value) -> Option<String> {
    let auth = entry.get("headers")?.get("Authorization")?.as_str()?;
    env_ref_name(auth.strip_prefix("Bearer ")?)
}

/// A worker `mcpServers` JSON entry as `(key, TOML value)` pairs — every
/// field the codex CLI understands: `url` + bearer/headers for HTTP
/// servers, `command`/`args`/`env` for stdio servers.
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

/// `key=value` pairs for `codex -c`, one per MCP server field: the
/// peckboard server plus every user-defined server riding in the host MCP
/// config (stdio and HTTP alike).
pub fn cli_config_overrides(wiring: &McpWiring) -> Vec<String> {
    let mut out = vec![
        format!("mcp_servers.{RESERVED}.url={}", toml_string(&wiring.url)),
        format!(
            "mcp_servers.{RESERVED}.bearer_token_env_var={}",
            toml_string(TOKEN_ENV_VAR)
        ),
        format!("mcp_servers.{RESERVED}.enabled=true"),
    ];
    for (name, entry) in &wiring.extra_servers {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn wiring(extras: Vec<(String, serde_json::Value)>) -> McpWiring {
        McpWiring {
            url: "http://127.0.0.1:4100/mcp".into(),
            token: "tok".into(),
            extra_servers: extras,
        }
    }

    fn gh_entry() -> (String, serde_json::Value) {
        (
            "github".to_string(),
            serde_json::json!({"type":"stdio","command":"npx","args":["-y","gh-mcp"],"env":{"GH_TOKEN":"x"}}),
        )
    }

    #[test]
    fn overrides_carry_stdio_servers_with_command_args_and_env() {
        let overrides = cli_config_overrides(&wiring(vec![
            gh_entry(),
            (
                "peckboard".to_string(),
                serde_json::json!({"command":"evil"}),
            ),
        ]));
        assert!(
            overrides.contains(&"mcp_servers.peckboard.url=\"http://127.0.0.1:4100/mcp\"".into())
        );
        assert!(overrides.contains(
            &"mcp_servers.peckboard.bearer_token_env_var=\"PECKBOARD_MCP_TOKEN\"".into()
        ));
        assert!(overrides.contains(&"mcp_servers.peckboard.enabled=true".into()));
        assert!(overrides.contains(&"mcp_servers.github.command=\"npx\"".into()));
        assert!(overrides.contains(&"mcp_servers.github.args=[\"-y\", \"gh-mcp\"]".into()));
        assert!(overrides.contains(&"mcp_servers.github.env={ GH_TOKEN = \"x\" }".into()));
        assert!(overrides.contains(&"mcp_servers.github.enabled=true".into()));
        assert!(!overrides.iter().any(|o| o.contains("evil")));
    }

    #[test]
    fn env_ref_bearer_becomes_bearer_token_env_var_and_keeps_other_headers() {
        let overrides = cli_config_overrides(&wiring(vec![(
            "linear".to_string(),
            serde_json::json!({
                "type": "http",
                "url": "https://api.example.com/mcp",
                "headers": {
                    "Authorization": "Bearer ${LINEAR_TOKEN}",
                    "X-Tenant": "acme",
                    "X-Trace": "${TRACE_ID}"
                }
            }),
        )]));
        assert!(
            overrides.contains(&"mcp_servers.linear.bearer_token_env_var=\"LINEAR_TOKEN\"".into())
        );
        assert!(
            overrides.contains(&"mcp_servers.linear.http_headers={ X-Tenant = \"acme\" }".into())
        );
        assert!(
            overrides
                .contains(&"mcp_servers.linear.env_http_headers={ X-Trace = \"TRACE_ID\" }".into())
        );
        assert!(!overrides.iter().any(|o| o.contains("Authorization")));
    }

    #[test]
    fn literal_bearer_stays_a_header_and_invents_no_env_binding() {
        let overrides = cli_config_overrides(&wiring(vec![(
            "linear".to_string(),
            serde_json::json!({
                "type": "http",
                "url": "https://api.example.com/mcp",
                "headers": { "Authorization": "Bearer sk-live-123" }
            }),
        )]));
        assert!(
            !overrides
                .iter()
                .any(|o| o.contains("linear.bearer_token_env_var"))
        );
        assert!(overrides.contains(
            &"mcp_servers.linear.http_headers={ Authorization = \"Bearer sk-live-123\" }".into()
        ));
    }

    #[test]
    fn unsafe_server_names_are_skipped() {
        let overrides = cli_config_overrides(&wiring(vec![(
            "bad name!".to_string(),
            serde_json::json!({"command":"npx"}),
        )]));
        assert!(!overrides.iter().any(|o| o.contains("bad name!")));
    }
}
