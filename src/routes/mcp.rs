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
            let tool_name = params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));

            // Token scoping enforcement: if the token is scoped to a project,
            // check that the tool call targets that same project.
            if let Some(ref scoped_pid) = token_project_id {
                if let Some(target_pid) = extract_target_project_id(tool_name, &arguments) {
                    if target_pid != *scoped_pid {
                        return (
                            StatusCode::FORBIDDEN,
                            rpc_json(JsonRpcResponse::error(
                                body.id,
                                -32000,
                                format!(
                                    "token scoped to project {scoped_pid}, cannot target {target_pid}"
                                ),
                            )),
                        );
                    }
                }
            }

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
            };

            match registry.handle_tool_call(tool_name, arguments, &ctx).await {
                Ok(result) => {
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
                Err(e) => (
                    StatusCode::OK,
                    rpc_json(JsonRpcResponse::error(
                        body.id,
                        -32000,
                        e.to_string(),
                    )),
                ),
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

/// For token scoping: extract the project ID that a tool call targets.
/// Returns None if the tool doesn't target a specific project (e.g. list_projects),
/// meaning session-scoped tokens already restrict via ctx.project_id.
fn extract_target_project_id(tool_name: &str, args: &Value) -> Option<String> {
    match tool_name {
        // Tools that reference a project_id directly
        "update_project" | "pause_project" | "resume_project" => {
            args.get("project_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }
        // Tools that reference a card; we'd need to look up the card's project
        // to fully enforce, but the card_id-based tools are already scoped
        // through ctx.project_id in the handler. For create_project the folder
        // is the scope, not a project. Return None for these to let the
        // handler-level checks apply.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer abc123".parse().unwrap(),
        );
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
    fn test_extract_target_project_id() {
        let args = serde_json::json!({ "project_id": "p-123" });
        assert_eq!(
            extract_target_project_id("update_project", &args),
            Some("p-123".to_string())
        );
        assert_eq!(
            extract_target_project_id("pause_project", &args),
            Some("p-123".to_string())
        );
        assert_eq!(
            extract_target_project_id("resume_project", &args),
            Some("p-123".to_string())
        );

        // Tools that don't directly reference project_id
        assert_eq!(
            extract_target_project_id("list_cards", &args),
            None
        );
        assert_eq!(
            extract_target_project_id("create_card", &args),
            None
        );
    }

    #[test]
    fn test_jsonrpc_response_success() {
        let resp = JsonRpcResponse::success(
            serde_json::json!(1),
            serde_json::json!({"ok": true}),
        );
        assert_eq!(resp.jsonrpc, "2.0");
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_jsonrpc_response_error() {
        let resp = JsonRpcResponse::error(
            serde_json::json!(1),
            -32600,
            "bad request".into(),
        );
        assert_eq!(resp.jsonrpc, "2.0");
        assert!(resp.result.is_none());
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32600);
    }
}
