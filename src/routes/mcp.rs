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
    id: Value,
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

    // Loopback gating: only allow from 127.0.0.1 or ::1
    let ip = addr.ip();
    if !ip.is_loopback() {
        return (
            StatusCode::FORBIDDEN,
            rpc_json(JsonRpcResponse::error(
                body.id,
                -32000,
                "loopback only".into(),
            )),
        );
    }

    // Validate JSON-RPC version
    if body.jsonrpc != "2.0" {
        return (
            StatusCode::BAD_REQUEST,
            rpc_json(JsonRpcResponse::error(
                body.id,
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
                    body.id,
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
                    body.id,
                    -32000,
                    "invalid MCP token".into(),
                )),
            );
        }
    };

    let registry = McpToolRegistry::new();

    match body.method.as_str() {
        "tools/list" => {
            let tools: Vec<Value> = registry
                .tool_definitions()
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.input_schema,
                    })
                })
                .collect();

            (
                StatusCode::OK,
                rpc_json(JsonRpcResponse::success(
                    body.id,
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

            // Look up the session to get card_id context
            let card_id = state
                .db
                .get_session(&session_id)
                .await
                .ok()
                .flatten()
                .and_then(|s| s.card_id);

            let ctx = ToolCallContext {
                session_id,
                project_id: token_project_id,
                card_id,
                db: Arc::new(state.db.clone()),
                broadcaster: state.broadcaster.clone(),
                provider_registry: Some(state.provider_registry.clone()),
                expert_dispatcher: Some(Arc::new(
                    crate::service::mcp_server::AppExpertDispatcher::new(state.clone()),
                )),
            };

            // ── Hook: mcp.tool.call.before ──
            let mut final_args = arguments;
            let hook_result = state
                .plugins
                .dispatch(
                    "mcp.tool.call.before",
                    serde_json::json!({
                        "sessionId": &ctx.session_id,
                        "toolName": tool_name,
                        "args": &final_args,
                    }),
                )
                .await;

            if let crate::plugin::hooks::HookResult::Cancelled { plugin, reason } = &hook_result {
                tracing::info!(plugin = %plugin, reason = %reason, "mcp.tool.call.before cancelled");
                return (
                    StatusCode::OK,
                    rpc_json(JsonRpcResponse::error(
                        body.id,
                        -32000,
                        format!("cancelled by plugin {plugin}: {reason}"),
                    )),
                );
            }
            // If a plugin modified the args, use the modified version
            if let crate::plugin::hooks::HookResult::Allowed(modified) = hook_result {
                if let Some(new_args) = modified.get("args") {
                    final_args = new_args.clone();
                }
            }

            match registry.handle_tool_call(tool_name, final_args, &ctx).await {
                Ok(result) => {
                    // ── Hook: mcp.tool.call.after ──
                    state
                        .plugins
                        .dispatch(
                            "mcp.tool.call.after",
                            serde_json::json!({
                                "sessionId": &ctx.session_id,
                                "toolName": tool_name,
                                "result": &result,
                            }),
                        )
                        .await;

                    let content = serde_json::json!([{
                        "type": "text",
                        "text": serde_json::to_string(&result).unwrap_or_default(),
                    }]);
                    (
                        StatusCode::OK,
                        rpc_json(JsonRpcResponse::success(
                            body.id,
                            serde_json::json!({ "content": content }),
                        )),
                    )
                }
                Err(e) => {
                    // ── Hook: mcp.tool.call.failed ──
                    state
                        .plugins
                        .dispatch(
                            "mcp.tool.call.failed",
                            serde_json::json!({
                                "sessionId": &ctx.session_id,
                                "toolName": tool_name,
                                "reason": e.to_string(),
                            }),
                        )
                        .await;

                    (
                        StatusCode::OK,
                        rpc_json(JsonRpcResponse::error(body.id, -32000, e.to_string())),
                    )
                }
            }
        }
        _ => (
            StatusCode::OK,
            rpc_json(JsonRpcResponse::error(
                body.id,
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
