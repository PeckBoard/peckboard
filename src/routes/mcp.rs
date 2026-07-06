use axum::{
    Json, Router,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode, header},
    routing::post,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::service::mcp_server::{McpToolRegistry, ToolCallContext};
use crate::state::AppState;

// ── JSON-RPC types ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    method: String,
    #[serde(default)]
    params: Option<Value>,
    // Absent for JSON-RPC notifications (e.g. `notifications/initialized`),
    // which carry no `id` and expect no response body.
    #[serde(default)]
    id: Option<Value>,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
    id: Value,
}

#[derive(Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

impl JsonRpcResponse {
    fn success(id: Value, result: Value) -> Self {
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            result: Some(result),
            error: None,
            id,
        }
    }

    fn error(id: Value, code: i32, message: String) -> Self {
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            result: None,
            error: Some(JsonRpcError {
                code,
                message,
                data: None,
            }),
            id,
        }
    }
}

// ── Router ─────────────────────────────────────────────────────────

/// MCP route -- not behind the normal auth middleware.
/// Uses its own bearer token auth and loopback gating.
pub fn router(_state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new().route("/mcp", post(mcp_handler))
}

// ── Handler ────────────────────────────────────────────────────────

async fn mcp_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<JsonRpcRequest>,
) -> (StatusCode, Json<Value>) {
    // Helper to convert JsonRpcResponse to Json<Value>
    let rpc_json =
        |resp: JsonRpcResponse| Json(serde_json::to_value(resp).unwrap_or(serde_json::json!({})));

    // Requests carry an `id`; notifications don't. For error responses (which
    // a notification never triggers a meaningful one for) fall back to null.
    let is_notification = body.id.is_none();
    let id = body.id.clone().unwrap_or(Value::Null);

    // Loopback gating: only allow from 127.0.0.1 or ::1
    let ip = addr.ip();
    if !ip.is_loopback() {
        return (
            StatusCode::FORBIDDEN,
            rpc_json(JsonRpcResponse::error(id, -32000, "loopback only".into())),
        );
    }

    // Validate JSON-RPC version
    if body.jsonrpc != "2.0" {
        return (
            StatusCode::BAD_REQUEST,
            rpc_json(JsonRpcResponse::error(
                id,
                -32600,
                "invalid jsonrpc version".into(),
            )),
        );
    }

    // Extract bearer token
    let token = match extract_bearer(&headers) {
        Some(t) => t,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                rpc_json(JsonRpcResponse::error(
                    id,
                    -32000,
                    "missing or invalid Authorization header".into(),
                )),
            );
        }
    };

    // Look up token in MCP registry
    let (session_id, token_project_id) = match state.mcp_tokens.lookup(&token).await {
        Some(info) => info,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                rpc_json(JsonRpcResponse::error(
                    id,
                    -32000,
                    "invalid MCP token".into(),
                )),
            );
        }
    };

    // Notifications (no `id`) — e.g. `notifications/initialized` after the
    // handshake — expect no JSON-RPC response, just an HTTP 202. This path
    // used to be answered locally by the node stdio proxy; it now lives here
    // so the CLI can speak HTTP transport straight to this route.
    if is_notification {
        return (StatusCode::ACCEPTED, Json(serde_json::json!({})));
    }

    let registry = McpToolRegistry::new();

    match body.method.as_str() {
        // MCP lifecycle handshake. Echo the client's requested protocol
        // version (or the documented default) and advertise the tools
        // capability — the only one this server implements.
        "initialize" => {
            let protocol_version = body
                .params
                .as_ref()
                .and_then(|p| p.get("protocolVersion"))
                .and_then(|v| v.as_str())
                .unwrap_or("2024-11-05")
                .to_string();
            (
                StatusCode::OK,
                rpc_json(JsonRpcResponse::success(
                    id,
                    serde_json::json!({
                        "protocolVersion": protocol_version,
                        "serverInfo": { "name": "peckboard", "version": "1.0.0" },
                        "capabilities": { "tools": {} },
                    }),
                )),
            )
        }
        "tools/list" => {
            // Every session gets a role-trimmed tool surface: workers lose
            // the admin tools, chats/experts lose the card-lifecycle and
            // worker-coordination tools — schemas they can never legitimately
            // use stop occupying context on every API call. Advertisement
            // only for those two lists; the per-handler scope checks remain
            // their enforcement point. Pre-hatcher research sessions instead
            // see ONLY the read-only allowlist — for them the same list is
            // also hard-enforced in `tools/call` below.
            let session_row = state.db.get_session(&session_id).await.ok().flatten();
            let is_worker = session_row.as_ref().map(|s| s.is_worker).unwrap_or(false);
            let pre_hatcher = session_row.as_ref().and_then(|s| s.expert_kind.as_deref())
                == Some(crate::service::mcp_server::PRE_HATCHER_EXPERT_KIND);
            let hidden: &[&str] = if is_worker {
                crate::service::mcp_server::worker_hidden_tool_names()
            } else {
                crate::service::mcp_server::chat_hidden_tool_names()
            };
            let advertised = |name: &str| {
                if pre_hatcher {
                    crate::service::mcp_server::pre_hatcher_allowed_tool_names().contains(&name)
                } else {
                    !hidden.contains(&name)
                }
            };
            let mut tools: Vec<Value> = registry
                .tool_definitions()
                .iter()
                .filter(|t| advertised(t.name.as_str()))
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.input_schema,
                    })
                })
                .collect();

            // Merge in tools contributed by active plugins. A core tool name
            // always wins — a plugin tool colliding with one is dropped with
            // a warning rather than shadowing core behaviour.
            let core_names: std::collections::HashSet<&str> = registry
                .tool_definitions()
                .iter()
                .map(|t| t.name.as_str())
                .collect();
            for t in state.plugins.mcp_tools().await {
                if core_names.contains(t.name.as_str()) {
                    tracing::warn!(
                        plugin = %t.plugin, tool = %t.name,
                        "plugin mcp_tool collides with a core tool name; dropping"
                    );
                    continue;
                }
                if !advertised(t.name.as_str()) {
                    continue;
                }
                tools.push(serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                }));
            }

            (
                StatusCode::OK,
                rpc_json(JsonRpcResponse::success(
                    id.clone(),
                    serde_json::json!({ "tools": tools }),
                )),
            )
        }
        "tools/call" => {
            let params = body.params.unwrap_or(serde_json::json!({}));
            let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));

            // Token-scope enforcement now lives inside each handler via
            // `ToolCallContext::scope_project` / `scope_card` /
            // `scope_session`, which produce a `ScopedProjectId` proof
            // token before any project- or card-scoped DB access. The
            // route layer is intentionally a no-op for scoping — the
            // earlier `extract_target_project_id` only covered three
            // tools and silently let `create_card`, `complete_step`,
            // `send_worker_message`, etc. bypass scope by passing a
            // different project/card/session id in the arguments.

            // Look up the session row once to derive both `card_id` and
            // `folder_id` for the tool-call context. The folder is the
            // load-bearing boundary every scope check enforces, so a
            // missing session row is fatal here — without a folder we'd
            // have to pick "deny everything" or "allow everything" by
            // default, and either way the call is broken.
            let session_row = match state.db.get_session(&session_id).await {
                Ok(Some(s)) => s,
                _ => {
                    return (
                        StatusCode::UNAUTHORIZED,
                        rpc_json(JsonRpcResponse::error(
                            id.clone(),
                            -32000,
                            "session not found for token".into(),
                        )),
                    );
                }
            };

            // HARD gate, not advertisement: a pre-hatcher research session
            // exists only to gather read-only context for the main model.
            // Its prompt already forbids mutation, but a prompt is advisory —
            // one pre-hatcher run ignored it and edited source files while
            // the main session worked the same repo. Anything outside the
            // read-only allowlist is refused here, whatever the model asks.
            if session_row.expert_kind.as_deref()
                == Some(crate::service::mcp_server::PRE_HATCHER_EXPERT_KIND)
                && !crate::service::mcp_server::pre_hatcher_allowed_tool_names()
                    .contains(&tool_name)
            {
                return (
                    StatusCode::OK,
                    rpc_json(JsonRpcResponse::error(
                        id.clone(),
                        -32000,
                        format!(
                            "tool '{tool_name}' is blocked: pre-hatcher sessions are \
                             read-only context gatherers. Use the read tools \
                             (read_file, search_files, file_outline, read_symbol, \
                             list_files) and hand off with pre_hatch_result; code \
                             changes are the main model's job."
                        ),
                    )),
                );
            }
            let card_id = session_row.card_id.clone();
            let folder_id = session_row.folder_id.clone();

            let ctx = ToolCallContext {
                session_id,
                project_id: token_project_id,
                card_id,
                folder_id,
                db: Arc::new(state.db.clone()),
                broadcaster: state.broadcaster.clone(),
                provider_registry: Some(state.provider_registry.clone()),
                data_dir: Some(state.config.data_dir.clone()),
            };

            // Run the call through the shared dispatcher: it fires the
            // `mcp.tool.call.before/after/failed` observer hooks and routes
            // the call to the owning plugin or core. A plugin cancellation in
            // the before-hook (or any handler failure) comes back as `Err`,
            // which we surface as a JSON-RPC error below.
            let tool_result = crate::service::mcp_server::dispatch_tool_call(
                &state.plugins,
                &registry,
                tool_name,
                arguments,
                &ctx,
            )
            .await;

            match tool_result {
                Ok(mut result) => {
                    // Handlers return an image (e.g. browser_screenshot) by
                    // embedding `_image_base64` (+ optional `_image_mime`):
                    // emitted as an MCP image content block so vision models
                    // see pixels, with the remaining fields as text.
                    let image = result.as_object_mut().and_then(|o| {
                        let data = o.remove("_image_base64")?;
                        let mime = o
                            .remove("_image_mime")
                            .and_then(|m| m.as_str().map(str::to_string))
                            .unwrap_or_else(|| "image/png".into());
                        Some((data, mime))
                    });
                    let text_block = serde_json::json!({
                        "type": "text",
                        "text": serde_json::to_string(&result).unwrap_or_default(),
                    });
                    let content = match image {
                        Some((data, mime)) => serde_json::json!([
                            { "type": "image", "data": data, "mimeType": mime },
                            text_block,
                        ]),
                        None => serde_json::json!([text_block]),
                    };
                    (
                        StatusCode::OK,
                        rpc_json(JsonRpcResponse::success(
                            id.clone(),
                            serde_json::json!({ "content": content }),
                        )),
                    )
                }
                Err(e) => (
                    StatusCode::OK,
                    rpc_json(JsonRpcResponse::error(id.clone(), -32000, e.to_string())),
                ),
            }
        }
        _ => (
            StatusCode::OK,
            rpc_json(JsonRpcResponse::error(
                id.clone(),
                -32601,
                format!("method not found: {}", body.method),
            )),
        ),
    }
}

/// Extract bearer token from the Authorization header.
fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?;
    let s = value.to_str().ok()?;
    let token = s.strip_prefix("Bearer ")?;
    Some(token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer abc123".parse().unwrap());
        assert_eq!(extract_bearer(&headers), Some("abc123".to_string()));
    }

    #[test]
    fn test_extract_bearer_missing() {
        let headers = HeaderMap::new();
        assert_eq!(extract_bearer(&headers), None);
    }

    #[test]
    fn test_extract_bearer_wrong_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert_eq!(extract_bearer(&headers), None);
    }

    #[test]
    fn test_jsonrpc_response_success() {
        let resp = JsonRpcResponse::success(serde_json::json!(1), serde_json::json!({"ok": true}));
        assert_eq!(resp.jsonrpc, "2.0");
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_jsonrpc_response_error() {
        let resp = JsonRpcResponse::error(serde_json::json!(1), -32600, "bad request".into());
        assert_eq!(resp.jsonrpc, "2.0");
        assert!(resp.result.is_none());
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32600);
    }
}
