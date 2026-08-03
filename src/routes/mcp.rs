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
            // Every session gets a role-trimmed tool surface via
            // `ToolGate` — the same hard-gate logic the Ollama in-process
            // tool path uses (see `service::mcp_server::gates`), so this
            // route and that path can't drift apart again. Advertisement
            // only here; the per-handler scope checks remain the boundary
            // enforcement point, and `tools/call` below re-checks the gate
            // as the hard enforcement point.
            let session_row = state.db.get_session(&session_id).await.ok().flatten();
            let plugin_tools = state.plugins.mcp_tools().await;
            let gate = session_row
                .as_ref()
                .map(crate::service::mcp_server::ToolGate::from_session)
                .unwrap_or_else(crate::service::mcp_server::ToolGate::none)
                .with_plugin_tools(&plugin_tools);
            let advertised = |name: &str| gate.advertised(name);
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
            for t in plugin_tools {
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

            // Hard gate, not advertisement: derive the same `ToolGate` used
            // in `tools/list` above — and by the Ollama in-process tool
            // path, so the two can't drift apart — and refuse outright if
            // it blocks this tool, whatever the model asked for by name.
            let gate = crate::service::mcp_server::ToolGate::from_session(&session_row)
                .with_plugin_tools(&state.plugins.mcp_tools().await);
            if let Some(reason) = gate.blocked(tool_name) {
                return (
                    StatusCode::OK,
                    rpc_json(JsonRpcResponse::error(id.clone(), -32000, reason)),
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
                    // `_begin_handover` (switch_session_model with
                    // compact:true): run the compacting handover to the new
                    // model here, where the AppState/session_manager it needs
                    // is in hand. The outgoing model writes a summary, the
                    // incoming one resumes on that compacted context. The
                    // marker is stripped so it never reaches the model.
                    if let Some((from, to)) = result.as_object_mut().and_then(|o| {
                        let hand = o.remove("_begin_handover")?;
                        let from = hand.get("from").and_then(|v| v.as_str())?.to_string();
                        let to = hand.get("to").and_then(|v| v.as_str())?.to_string();
                        Some((from, to))
                    }) {
                        if let Err(e) = crate::handover::begin_handover(
                            &state,
                            &ctx.session_id,
                            &from,
                            &to,
                            None,
                        )
                        .await
                        {
                            tracing::warn!(
                                session_id = %ctx.session_id,
                                error = %e,
                                "compacting handover dispatch failed"
                            );
                        }
                    }
                    // `_dispatch_session` (spawn_subagent): drive the freshly
                    // created child session's first turn here, where the
                    // AppState-backed dispatcher lives. Fire-and-forget so the
                    // tool result returns to the caller immediately; the
                    // prompt is already persisted as the child's user event.
                    if let Some((child_id, text)) = result.as_object_mut().and_then(|o| {
                        let d = o.remove("_dispatch_session")?;
                        let sid = d.get("session_id").and_then(|v| v.as_str())?.to_string();
                        let text = d.get("text").and_then(|v| v.as_str())?.to_string();
                        Some((sid, text))
                    }) {
                        let dispatcher =
                            crate::service::mcp_server::AppExpertDispatcher::new(state.clone());
                        tokio::spawn(async move {
                            use crate::service::mcp_server::ExpertDispatcher;
                            if let Err(e) = dispatcher.resume_session(&child_id, &text).await {
                                tracing::warn!(
                                    session_id = %child_id,
                                    "subagent first-turn dispatch failed: {e}"
                                );
                            }
                        });
                    }
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
