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

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

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
        if name == RESERVED {
            continue;
        }
        if let Some(url) = entry.get("url").and_then(|v| v.as_str()) {
            out.push(format!("mcp_servers.{name}.url={}", toml_string(url)));
            out.push(format!("mcp_servers.{name}.enabled=true"));
        }
    }
    out
}
