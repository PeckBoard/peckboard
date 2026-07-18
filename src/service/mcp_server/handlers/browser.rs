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

/// Viewport screenshot of `page_id` as base64 PNG — the frame source for
/// run recording. Best-effort: recording must never fail a tool call, and
/// frames never enter model context (server-side only).
async fn capture_frame(page_id: &str) -> Option<String> {
    let r = browser::post(
        &format!("/api/pages/{page_id}/screenshot"),
        serde_json::json!({ "fullPage": false }),
    )
    .await
    .ok()?;
    let shot = r.get("screenshot")?.as_str()?;
    Some(shot.rsplit(',').next().unwrap_or(shot).to_string())
}

/// Drain new capture events (network/console) from the sidecar into the
/// page's run, masked before persisting. Best-effort and recording-gated:
/// when the page isn't recorded (or the sidecar runs in its no-capture
/// fallback) this is a no-op.
async fn capture_events(page_id: &str) {
    let Some(cursor) = crate::service::browser_runs::events_cursor(page_id) else {
        return;
    };
    if let Ok(v) = browser::get(&format!("/api/pages/{page_id}/events?since={cursor}")).await {
        crate::service::browser_runs::ingest_events(page_id, &v);
    }
}

impl McpToolRegistry {
    pub(crate) async fn handle_browser_tool(
        &self,
        tool_name: &str,
        args: Value,
        ctx: &ToolCallContext,
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
                // Start run recording (playwright-video replay).
                if let Some(data_dir) = ctx.data_dir.as_deref() {
                    crate::service::browser_runs::start(
                        data_dir,
                        &page_id,
                        name,
                        url,
                        &ctx.session_id,
                        ctx.project_id.as_deref(),
                        ctx.card_id.as_deref(),
                    );
                    let frame = capture_frame(&page_id).await;
                    crate::service::browser_runs::record_step(
                        &page_id,
                        "open",
                        None,
                        Some(serde_json::json!({ "url": url })),
                        frame.as_deref(),
                    );
                    capture_events(&page_id).await;
                }
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
                    "select" => {
                        // Multi-select passes `values` (array); single select `text`.
                        let value = args
                            .get("values")
                            .filter(|v| v.is_array())
                            .cloned()
                            .or_else(|| args.get("text").cloned())
                            .unwrap_or(Value::Null);
                        (
                            format!("/api/pages/{page_id}/select"),
                            serde_json::json!({ "ref": need_ref()?, "value": value, "element": format!("ref={}", refe.unwrap_or_default()) }),
                        )
                    }
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
                    "upload" => {
                        let files = args
                            .get("files")
                            .filter(|v| v.as_array().is_some_and(|a| !a.is_empty()))
                            .cloned()
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "`files` (non-empty array of file paths) is required for upload"
                                )
                            })?;
                        (
                            format!("/api/pages/{page_id}/upload"),
                            serde_json::json!({ "ref": need_ref()?, "files": files }),
                        )
                    }
                    other => anyhow::bail!(
                        "unknown action `{other}` (use click|type|fill|select|hover|press_key|\
                         upload|navigate|back|forward|scroll_top|scroll_bottom|wait_selector|\
                         wait_ms|dialog)"
                    ),
                };
                let r = browser::post(&path, body).await?;
                // Step + fresh frame for replay. Gated on data_dir (the
                // recording precondition) so unrecorded pages skip the
                // screenshot round-trip entirely.
                if ctx.data_dir.is_some() {
                    let frame = capture_frame(page_id).await;
                    let mut detail = serde_json::Map::new();
                    for k in ["text", "values", "files", "timeout_ms", "accept"] {
                        if let Some(v) = args.get(k) {
                            detail.insert(k.to_string(), v.clone());
                        }
                    }
                    // Typed input is never stored raw — the target may be a
                    // password field, indistinguishable from here, so mask it
                    // wholesale like session-replay tools do.
                    if matches!(action, "fill" | "type")
                        && let Some(v) = detail.get_mut("text")
                    {
                        *v = Value::String(crate::service::redact::MASK.to_string());
                    }
                    crate::service::browser_runs::record_step(
                        page_id,
                        action,
                        refe,
                        (!detail.is_empty()).then_some(Value::Object(detail)),
                        frame.as_deref(),
                    );
                    capture_events(page_id).await;
                }
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
                crate::service::browser_runs::record_step(
                    page_id,
                    "screenshot",
                    None,
                    None,
                    Some(&b64),
                );
                capture_events(page_id).await;
                // `_image_base64` is the routes/mcp.rs convention for
                // returning an MCP image content block.
                Ok(serde_json::json!({
                    "page_id": page_id,
                    "full_page": full,
                    "_image_base64": b64,
                    "_image_mime": "image/png",
                }))
            }

            // Mirror of upstream `listPages`.
            "browser_pages" => {
                let r = browser::get("/api/pages").await?;
                Ok(serde_json::json!({ "pages": r }))
            }

            "browser_close" => {
                let page_id = req_str(&args, "page_id")?;
                browser::delete(&format!("/api/pages/{page_id}")).await?;
                capture_events(page_id).await;
                crate::service::browser_runs::finish(page_id);
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

    fn ctx_with(data_dir: Option<std::path::PathBuf>) -> ToolCallContext {
        ToolCallContext {
            session_id: "s-1".into(),
            project_id: None,
            card_id: None,
            folder_id: "f-1".into(),
            db: Arc::new(crate::db::Db::in_memory().unwrap()),
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
            provider_registry: None,
            data_dir,
        }
    }

    fn ctx() -> ToolCallContext {
        ctx_with(None)
    }

    /// Both tests drive the same global page-id ("p1") through the global
    /// `browser_runs` registry — serialize them so runs don't interleave.
    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        static M: std::sync::Mutex<()> = std::sync::Mutex::new(());
        M.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Stub of the sidecar HTTP API (upstream routes + `/events`). One
    /// server for the whole process, on its own runtime thread — tests share
    /// it because the base-url override is a process-global OnceLock, so it
    /// must outlive every test's runtime.
    fn stub_router() -> axum::Router {
        use axum::Json;
        use axum::routing::{delete, post};
        axum::Router::new()
            .route(
                "/api/pages",
                post(|| async { Json(serde_json::json!({ "pageId": "p1", "success": true })) })
                    .get(|| async {
                        Json(serde_json::json!([
                            { "id": "p1", "name": "page", "url": "https://example.com", "title": "Hi" }
                        ]))
                    }),
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
                "/api/pages/p1/select",
                post(|Json(b): Json<serde_json::Value>| async move {
                    Json(serde_json::json!({ "success": true, "value": b["value"] }))
                }),
            )
            .route(
                "/api/pages/p1/upload",
                post(|Json(b): Json<serde_json::Value>| async move {
                    Json(serde_json::json!({ "success": true, "files": b["files"] }))
                }),
            )
            .route(
                "/api/pages/p1/fill",
                post(|Json(b): Json<serde_json::Value>| async move {
                    Json(serde_json::json!({ "success": true, "ref": b["ref"] }))
                }),
            )
            .route(
                "/api/pages/p1/screenshot",
                post(|| async { Json(serde_json::json!({ "screenshot": "aGVsbG8=" })) }),
            )
            .route(
                "/api/pages/p1/events",
                axum::routing::get(
                    |q: axum::extract::Query<std::collections::HashMap<String, String>>| async move {
                        let since: u64 = q.get("since").and_then(|s| s.parse().ok()).unwrap_or(0);
                        if since >= 3 {
                            return Json(serde_json::json!({ "events": [], "next": 3, "dropped": 0 }));
                        }
                        Json(serde_json::json!({
                            "next": 3,
                            "dropped": 0,
                            "events": [
                                { "seq": 1, "kind": "net-req", "id": 7, "ts": 1000, "method": "POST",
                                  "url": "https://api.example/login?access_token=hush",
                                  "resourceType": "xhr",
                                  "headers": { "authorization": "Bearer supersecret99", "content-type": "application/json" },
                                  "postData": "{\"user\":\"jo\",\"password\":\"hunter2\"}" },
                                { "seq": 2, "kind": "net-fin", "id": 7, "ts": 1400, "status": 200,
                                  "headers": { "set-cookie": "sid=abc", "content-type": "application/json" },
                                  "body": "{\"ok\":true,\"token\":\"tok-1\"}", "size": 27 },
                                { "seq": 3, "kind": "console", "ts": 1500, "level": "error",
                                  "text": "auth failed for Bearer zzzz11112222" }
                            ]
                        }))
                    },
                ),
            )
            .route(
                "/api/pages/p1",
                delete(|| async { Json(serde_json::json!({ "success": true })) }),
            )
    }

    fn start_stub() -> String {
        static BASE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        BASE.get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async move {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    tx.send(format!("http://{}", listener.local_addr().unwrap()))
                        .unwrap();
                    axum::serve(listener, stub_router()).await.unwrap();
                });
            });
            rx.recv().unwrap()
        })
        .clone()
    }

    #[tokio::test]
    async fn browser_tools_map_onto_the_http_api() {
        let _g = test_guard();
        let base = start_stub();
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
        let pages = reg
            .handle_tool_call("browser_pages", serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert_eq!(pages["pages"][0]["id"], "p1", "got: {pages}");

        let selected = reg
            .handle_tool_call(
                "browser_act",
                serde_json::json!({ "page_id": "p1", "action": "select", "ref": "e2", "values": ["a", "b"] }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(selected["success"], true);

        let uploaded = reg
            .handle_tool_call(
                "browser_act",
                serde_json::json!({ "page_id": "p1", "action": "upload", "ref": "e3", "files": ["/tmp/a.png"] }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(uploaded["success"], true);

        // upload without files is a clear error.
        let err = reg
            .handle_tool_call(
                "browser_act",
                serde_json::json!({ "page_id": "p1", "action": "upload", "ref": "e3" }),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("`files`"), "got: {err}");

        assert_eq!(closed["closed"], "p1");
    }

    #[tokio::test]
    async fn capture_events_land_masked_in_the_run() {
        let _g = test_guard();
        let base = start_stub();
        crate::service::browser::set_test_base_url(&base);
        let dir = tempfile::tempdir().unwrap();
        let reg = McpToolRegistry::new();
        let ctx = ctx_with(Some(dir.path().to_path_buf()));

        reg.handle_tool_call(
            "browser_open",
            serde_json::json!({ "url": "https://example.com" }),
            &ctx,
        )
        .await
        .unwrap();
        reg.handle_tool_call(
            "browser_act",
            serde_json::json!({ "page_id": "p1", "action": "fill", "ref": "e9", "text": "hunter2-typed-pw" }),
            &ctx,
        )
        .await
        .unwrap();
        reg.handle_tool_call(
            "browser_close",
            serde_json::json!({ "page_id": "p1" }),
            &ctx,
        )
        .await
        .unwrap();

        let runs = crate::service::browser_runs::list_runs(dir.path());
        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert!(run.ended_ms.is_some());
        assert_eq!(run.steps[0].action, "open");
        // Typed input is stored fully masked — the field may be a password.
        let fill = run.steps.iter().find(|s| s.action == "fill").unwrap();
        let detail = serde_json::to_string(&fill.detail).unwrap();
        assert!(!detail.contains("hunter2-typed-pw"), "got: {detail}");
        assert!(detail.contains("masked"), "got: {detail}");
        assert_eq!(run.network.len(), 1);
        let ne = &run.network[0];
        assert_eq!(ne.status, Some(200));
        assert_eq!(ne.dur_ms, Some(400));
        assert_eq!(
            ne.req_headers["authorization"],
            crate::service::redact::MASK
        );
        assert_eq!(ne.resp_headers["set-cookie"], crate::service::redact::MASK);
        assert!(ne.url.ends_with("access_token=«masked»"), "url: {}", ne.url);
        assert!(!ne.req_body.as_deref().unwrap_or("").contains("hunter2"));
        assert!(!ne.resp_body.as_deref().unwrap_or("").contains("tok-1"));
        assert_eq!(run.console_events.len(), 1);
        assert!(!run.console_events[0].text.contains("zzzz11112222"));
    }
}
