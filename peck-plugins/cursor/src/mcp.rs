//! Parse the host MCP config and emit workspace files as strings.

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

pub fn is_safe_server_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub fn workspace_mcp_json(wiring: &McpWiring) -> String {
    let mut servers = serde_json::Map::new();
    servers.insert(
        RESERVED.into(),
        serde_json::json!({
            "url": wiring.url,
            "headers": { "Authorization": format!("Bearer ${{{TOKEN_ENV_VAR}}}") }
        }),
    );
    for (name, entry) in &wiring.extra_servers {
        if is_safe_server_name(name) {
            servers.insert(name.clone(), entry.clone());
        }
    }
    serde_json::json!({ "mcpServers": servers }).to_string()
}
