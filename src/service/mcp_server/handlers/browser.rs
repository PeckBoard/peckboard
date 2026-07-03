//! `browser_*` tool handlers — headless-browser testing via the managed
//! better-playwright-mcp3 server (see `crate::service::browser`). Token-lean
//! by design: compressed outlines + snapshot regex search, never raw DOM.

use serde_json::Value;

use super::super::McpToolRegistry;
use super::super::context::ToolCallContext;
use crate::service::browser;

fn req_str<'a>(args: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("`{key}` is required"))
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

/// Fetch the compressed outline for a page.
async fn outline(page_id: &str) -> anyhow::Result<String> {
    let r = browser::post(
        &format!("/api/pages/{page_id}/outline"),
        serde_json::json!({}),
    )
    .await?;
    Ok(r.get("outline")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string())
}

impl McpToolRegistry {
    pub(crate) async fn handle_browser_tool(
        &self,
        tool_name: &str,
        args: Value,
        _ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        match tool_name {
            // Create a page, navigate, and return the compressed outline in
            // one round-trip.
            "browser_open" => {
                let url = req_str(&args, "url")?;
                let name = opt_str(&args, "name").unwrap_or("page");
                let created = browser::post(
                    "/api/pages",
                    serde_json::json!({
                        "name": name,
                        "description": format!("peckboard test page: {name}"),
                        "url": url,
                    }),
                )
                .await?;
                let page_id = created
                    .get("pageId")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("browser server returned no pageId: {created}"))?
                    .to_string();
                let outline = outline(&page_id).await.unwrap_or_default();
                Ok(serde_json::json!({ "page_id": page_id, "url": url, "outline": outline }))
            }

            "browser_outline" => {
                let page_id = req_str(&args, "page_id")?;
                Ok(serde_json::json!({ "outline": outline(page_id).await? }))
            }

            "browser_find" => {
                let page_id = req_str(&args, "page_id")?;
                let pattern = req_str(&args, "pattern")?;
                let r = browser::post(
                    &format!("/api/pages/{page_id}/search"),
                    serde_json::json!({
                        "pattern": pattern,
                        "ignoreCase": args.get("ignore_case").and_then(|v| v.as_bool()).unwrap_or(true),
                        "lineLimit": args.get("line_limit").and_then(|v| v.as_u64()).unwrap_or(50).min(100),
                    }),
                )
                .await?;
                Ok(serde_json::json!({
                    "result": r.get("result").cloned().unwrap_or(Value::Null),
                    "match_count": r.get("matchCount").cloned().unwrap_or(Value::Null),
                    "truncated": r.get("truncated").cloned().unwrap_or(Value::Null),
                }))
            }

            // One dispatch tool for every interaction — fewer schemas in
            // every session's context than one tool per action.
            "browser_act" => {
                let page_id = req_str(&args, "page_id")?;
                let action = req_str(&args, "action")?;
                let refe = opt_str(&args, "ref");
                let need_ref = || -> anyhow::Result<&str> {
                    refe.ok_or_else(|| anyhow::anyhow!("`ref` is required for action `{action}`"))
                };
                let (path, body): (String, Value) = match action {
                    "click" => (
                        format!("/api/pages/{page_id}/click"),
                        serde_json::json!({ "ref": need_ref()?, "element": format!("ref={}", refe.unwrap_or_default()) }),
                    ),
                    "type" => (
                        format!("/api/pages/{page_id}/type"),
                        serde_json::json!({ "ref": need_ref()?, "text": req_str(&args, "text")?, "element": format!("ref={}", refe.unwrap_or_default()) }),
                    ),
                    "fill" => (
                        format!("/api/pages/{page_id}/fill"),
                        serde_json::json!({ "ref": need_ref()?, "value": req_str(&args, "text")?, "element": format!("ref={}", refe.unwrap_or_default()) }),
                    ),
                    "select" => (
                        format!("/api/pages/{page_id}/select"),
                        serde_json::json!({ "ref": need_ref()?, "value": args.get("text").cloned().unwrap_or(Value::Null), "element": format!("ref={}", refe.unwrap_or_default()) }),
                    ),
                    "hover" => (
                        format!("/api/pages/{page_id}/hover"),
                        serde_json::json!({ "ref": need_ref()?, "element": format!("ref={}", refe.unwrap_or_default()) }),
                    ),
                    "press_key" => (
                        format!("/api/pages/{page_id}/press"),
                        serde_json::json!({ "key": req_str(&args, "text")? }),
                    ),
                    "navigate" => (
                        format!("/api/pages/{page_id}/navigate"),
                        serde_json::json!({ "url": req_str(&args, "text")? }),
                    ),
                    "back" => (format!("/api/pages/{page_id}/back"), serde_json::json!({})),
                    "forward" => (
                        format!("/api/pages/{page_id}/forward"),
                        serde_json::json!({}),
                    ),
                    "scroll_top" => (
                        format!("/api/pages/{page_id}/scroll-top"),
                        serde_json::json!({ "ref": refe }),
                    ),
                    "scroll_bottom" => (
                        format!("/api/pages/{page_id}/scroll-bottom"),
                        serde_json::json!({ "ref": refe }),
                    ),
                    "wait_selector" => (
                        format!("/api/pages/{page_id}/wait-selector"),
                        serde_json::json!({ "selector": req_str(&args, "text")? }),
                    ),
                    "wait_ms" => (
                        format!("/api/pages/{page_id}/wait-timeout"),
                        serde_json::json!({ "timeout": args.get("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(1000).min(30_000) }),
                    ),
                    "dialog" => (
                        format!("/api/pages/{page_id}/dialog"),
                        serde_json::json!({ "accept": args.get("accept").and_then(|v| v.as_bool()).unwrap_or(true), "text": opt_str(&args, "text") }),
                    ),
                    other => anyhow::bail!(
                        "unknown action `{other}` (use click|type|fill|select|hover|press_key|\
                         navigate|back|forward|scroll_top|scroll_bottom|wait_selector|wait_ms|dialog)"
                    ),
                };
                let r = browser::post(&path, body).await?;
                let mut out = serde_json::json!({
                    "success": r.get("success").cloned().unwrap_or(Value::Bool(true)),
                    "action": action,
                });
                // The outline costs ~2k tokens — attach it only on request.
                if args
                    .get("outline")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                    && let Ok(o) = outline(page_id).await
                    && let Some(obj) = out.as_object_mut()
                {
                    obj.insert("outline".into(), Value::String(o));
                }
                Ok(out)
            }

            "browser_screenshot" => {
                let page_id = req_str(&args, "page_id")?;
                let full = args
                    .get("full_page")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let r = browser::post(
                    &format!("/api/pages/{page_id}/screenshot"),
                    serde_json::json!({ "fullPage": full }),
                )
                .await?;
                let shot = r
                    .get("screenshot")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("browser server returned no screenshot"))?;
                // Defensive: strip a data-URL prefix if the server sends one.
                let b64 = shot.rsplit(',').next().unwrap_or(shot).to_string();
                // `_image_base64` is the routes/mcp.rs convention for
                // returning an MCP image content block.
                Ok(serde_json::json!({
                    "page_id": page_id,
                    "full_page": full,
                    "_image_base64": b64,
                    "_image_mime": "image/png",
                }))
            }

            "browser_close" => {
                let page_id = req_str(&args, "page_id")?;
                browser::delete(&format!("/api/pages/{page_id}")).await?;
                Ok(serde_json::json!({ "closed": page_id }))
            }

            other => anyhow::bail!("unknown browser tool: {other}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::super::McpToolRegistry;
    use super::super::super::context::ToolCallContext;

    fn ctx() -> ToolCallContext {
        ToolCallContext {
            session_id: "s-1".into(),
            project_id: None,
            card_id: None,
            folder_id: "f-1".into(),
            db: Arc::new(crate::db::Db::in_memory().unwrap()),
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
            provider_registry: None,
            data_dir: None,
        }
    }

    /// Stub of the better-playwright-mcp3 HTTP API. One server for the whole
    /// test — the base-url override is a process-global OnceLock.
    async fn start_stub() -> String {
        use axum::Json;
        use axum::routing::{delete, post};
        let app = axum::Router::new()
            .route(
                "/api/pages",
                post(|| async { Json(serde_json::json!({ "pageId": "p1", "success": true })) }),
            )
            .route(
                "/api/pages/p1/outline",
                post(|| async {
                    Json(serde_json::json!({ "outline": "- heading \"Hi\" [ref=e1]" }))
                }),
            )
            .route(
                "/api/pages/p1/search",
                post(|Json(b): Json<serde_json::Value>| async move {
                    Json(serde_json::json!({
                        "result": format!("match for {}", b["pattern"].as_str().unwrap_or("")),
                        "matchCount": 1,
                        "truncated": false,
                    }))
                }),
            )
            .route(
                "/api/pages/p1/click",
                post(|Json(b): Json<serde_json::Value>| async move {
                    Json(serde_json::json!({ "success": true, "ref": b["ref"] }))
                }),
            )
            .route(
                "/api/pages/p1/screenshot",
                post(|| async { Json(serde_json::json!({ "screenshot": "aGVsbG8=" })) }),
            )
            .route(
                "/api/pages/p1",
                delete(|| async { Json(serde_json::json!({ "success": true })) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn browser_tools_map_onto_the_http_api() {
        let base = start_stub().await;
        crate::service::browser::set_test_base_url(&base);
        let reg = McpToolRegistry::new();
        let ctx = ctx();

        let opened = reg
            .handle_tool_call(
                "browser_open",
                serde_json::json!({ "url": "https://example.com" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(opened["page_id"], "p1");
        assert!(opened["outline"].as_str().unwrap().contains("ref=e1"));

        let found = reg
            .handle_tool_call(
                "browser_find",
                serde_json::json!({ "page_id": "p1", "pattern": "Hi" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(found["match_count"], 1);

        let acted = reg
            .handle_tool_call(
                "browser_act",
                serde_json::json!({ "page_id": "p1", "action": "click", "ref": "e1", "outline": true }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(acted["success"], true);
        assert!(acted["outline"].as_str().unwrap().contains("Hi"));

        // Missing ref for an element action is a clear error.
        let err = reg
            .handle_tool_call(
                "browser_act",
                serde_json::json!({ "page_id": "p1", "action": "click" }),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("`ref` is required"), "got: {err}");

        let shot = reg
            .handle_tool_call(
                "browser_screenshot",
                serde_json::json!({ "page_id": "p1" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(shot["_image_base64"], "aGVsbG8=");
        assert_eq!(shot["_image_mime"], "image/png");

        let closed = reg
            .handle_tool_call(
                "browser_close",
                serde_json::json!({ "page_id": "p1" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(closed["closed"], "p1");
    }
}
