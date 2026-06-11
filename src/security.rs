use axum::{
    extract::Request,
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Security headers middleware — adds CSP, X-Content-Type-Options, X-Frame-Options.
pub async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();

    // No `style-src-attr 'unsafe-inline'`: inline `style=""` attributes
    // are a CSS-based exfiltration vector when combined with any other
    // XSS toehold, and the React app uses class/CSS-module styling
    // rather than dynamic inline `style` for almost everything.
    headers.insert(
        "Content-Security-Policy",
        HeaderValue::from_static(
            "default-src 'self'; \
             script-src 'self'; \
             style-src 'self'; \
             img-src 'self' data: blob:; \
             connect-src 'self'; \
             frame-ancestors 'none'; \
             object-src 'none'",
        ),
    );

    headers.insert(
        "X-Content-Type-Options",
        HeaderValue::from_static("nosniff"),
    );

    headers.insert("X-Frame-Options", HeaderValue::from_static("DENY"));

    headers.insert(
        "Referrer-Policy",
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );

    response
}

/// Origin/CSRF protection middleware.
/// Compares Origin header against Host header.
/// Absent Origin is treated as same-origin.
pub async fn origin_check(request: Request, next: Next) -> Response {
    let origin = request.headers().get(header::ORIGIN).cloned();
    let host = request.headers().get(header::HOST).cloned();

    // If Origin is absent, treat as same-origin (curl, MCP subprocess, etc.)
    if origin.is_none() {
        return next.run(request).await;
    }

    let origin_str = origin.unwrap();
    let origin_val = origin_str.to_str().unwrap_or("");

    if let Some(host_val) = host {
        let host_str = host_val.to_str().unwrap_or("");
        // Extract hostname from Origin URL (strip protocol)
        let origin_host = origin_val
            .strip_prefix("http://")
            .or_else(|| origin_val.strip_prefix("https://"))
            .unwrap_or(origin_val);

        // Compare hostnames only (strip port) — allows different ports on same host
        // (e.g. Vite dev server on :5173 talking to backend on :3333)
        let origin_hostname = origin_host.split(':').next().unwrap_or(origin_host);
        let host_hostname = host_str.split(':').next().unwrap_or(host_str);

        if origin_hostname.eq_ignore_ascii_case(host_hostname) {
            return next.run(request).await;
        }
    }

    // Note: there is no `/mcp` carve-out here. MCP clients run as
    // subprocesses on the loopback interface and don't send an Origin
    // header at all (they hit the absent-Origin branch above), so they
    // pass without needing an exception. A browser hitting `/mcp` from
    // a foreign origin is exactly the case we want to block.
    (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({"error": "cross-origin request blocked"})),
    )
        .into_response()
}

/// Body size limit constant: 20 MB for JSON bodies.
pub const MAX_JSON_BODY_SIZE: usize = 20 * 1024 * 1024;

/// Startup state repair: detect dangling agent-starts and synthesize agent-end events.
pub async fn repair_dangling_sessions(db: &crate::db::Db) -> anyhow::Result<u32> {
    // Get all sessions
    let sessions = db.list_sessions().await?;
    let mut repaired = 0u32;

    for session in sessions {
        // Check the latest event
        let tail = db.events_tail(&session.id, 1).await?;
        if let Some(last_event) = tail.first() {
            if last_event.kind == "agent-start" {
                // Dangling agent-start — synthesize a crashed agent-end
                db.append_event(
                    &session.id,
                    "agent-end",
                    serde_json::json!({
                        "status": "crashed",
                        "reason": "server-shutdown",
                    }),
                )
                .await?;
                repaired += 1;
                tracing::warn!("Repaired dangling agent-start for session {}", session.id);
            }
        }
    }

    if repaired > 0 {
        tracing::info!("Repaired {repaired} dangling session(s)");
    }

    // Resume interrupted workers: at startup no processes are running, so
    // any non-terminal, non-blocked card with a worker_session_id has a dead
    // worker. Clear the ref so the orchestrator can re-spawn them.
    let projects = db.list_projects().await.unwrap_or_default();
    let mut workers_recovered = 0u32;

    for project in &projects {
        if project.status != "active" {
            continue;
        }

        let cards = db
            .list_cards_by_project(&project.id)
            .await
            .unwrap_or_default();
        for card in &cards {
            if card.step == "done" || card.step == "wont_do" {
                continue;
            }
            let session_id = match &card.worker_session_id {
                Some(sid) => sid.clone(),
                None => continue,
            };
            if card.blocked {
                continue;
            }

            // At startup, no processes are alive. Clear the worker ref.
            let now = chrono::Utc::now().to_rfc3339();
            let _ = db
                .update_card(
                    &card.id,
                    crate::db::models::UpdateCard {
                        worker_session_id: Some(None),
                        updated_at: Some(now),
                        ..Default::default()
                    },
                )
                .await;
            workers_recovered += 1;
            tracing::info!(
                card_id = %card.id,
                session_id = %session_id,
                "Recovering interrupted worker for card \"{}\"",
                card.title
            );
        }
    }

    if workers_recovered > 0 {
        tracing::info!("Recovered {workers_recovered} interrupted worker(s) for re-spawning");
    }

    Ok(repaired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::db::models::{NewCard, NewFolder, NewProject, NewSession, UpdateCard};
    use axum::{Router, body::Body, http::Request, middleware, routing::get};
    use tower::ServiceExt;

    async fn send(app: Router, request: Request<Body>) -> Response {
        app.oneshot(request).await.unwrap()
    }

    #[tokio::test]
    async fn security_headers_are_added() {
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(middleware::from_fn(security_headers));

        let response = send(
            app,
            Request::builder().uri("/").body(Body::empty()).unwrap(),
        )
        .await;
        let headers = response.headers();

        let csp = headers
            .get("Content-Security-Policy")
            .and_then(|v| v.to_str().ok())
            .unwrap();
        assert!(csp.contains("default-src 'self'"));
        assert!(csp.contains("frame-ancestors 'none'"));
        assert_eq!(headers.get("X-Content-Type-Options").unwrap(), "nosniff");
        assert_eq!(headers.get("X-Frame-Options").unwrap(), "DENY");
        assert_eq!(
            headers.get("Referrer-Policy").unwrap(),
            "strict-origin-when-cross-origin"
        );
    }

    fn origin_app() -> Router {
        Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(middleware::from_fn(origin_check))
    }

    fn origin_request(origin: Option<&str>, host: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().uri("/");
        if let Some(o) = origin {
            builder = builder.header(header::ORIGIN, o);
        }
        if let Some(h) = host {
            builder = builder.header(header::HOST, h);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn origin_check_allows_absent_origin() {
        // curl / MCP subprocess case
        let response = send(origin_app(), origin_request(None, Some("localhost:3344"))).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn origin_check_allows_same_host_any_port() {
        for origin in [
            "http://localhost:3344",
            "https://localhost:3345",
            "http://localhost:5173", // Vite dev server
            "http://LOCALHOST:3344", // case-insensitive
        ] {
            let response = send(
                origin_app(),
                origin_request(Some(origin), Some("localhost:3344")),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK, "origin {origin}");
        }
    }

    #[tokio::test]
    async fn origin_check_blocks_cross_origin() {
        for origin in ["http://evil.example.com", "https://evil.example.com:3344"] {
            let response = send(
                origin_app(),
                origin_request(Some(origin), Some("localhost:3344")),
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "origin {origin}");
        }
    }

    #[tokio::test]
    async fn origin_check_blocks_when_host_header_missing() {
        let response = send(
            origin_app(),
            origin_request(Some("http://localhost:3344"), None),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    // ── repair_dangling_sessions ─────────────────────────────────────

    async fn seed_session(db: &Db, id: &str) {
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_session(NewSession {
            id: id.into(),
            name: format!("Session {id}"),
            folder_id: "f1".into(),
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn repairs_dangling_agent_start() {
        let db = Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts,
        })
        .await
        .unwrap();

        // s1 ends with a dangling agent-start; s2 ended cleanly.
        seed_session(&db, "s1").await;
        seed_session(&db, "s2").await;
        db.append_event("s1", "agent-start", serde_json::json!({}))
            .await
            .unwrap();
        db.append_event("s2", "agent-start", serde_json::json!({}))
            .await
            .unwrap();
        db.append_event("s2", "agent-end", serde_json::json!({"status": "done"}))
            .await
            .unwrap();

        let repaired = repair_dangling_sessions(&db).await.unwrap();
        assert_eq!(repaired, 1);

        let tail = db.events_tail("s1", 1).await.unwrap();
        assert_eq!(tail[0].kind, "agent-end");
        let data: serde_json::Value = serde_json::from_str(&tail[0].data).unwrap();
        assert_eq!(data["status"], "crashed");

        // Clean session untouched: still exactly two events.
        let s2_tail = db.events_tail("s2", 10).await.unwrap();
        assert_eq!(s2_tail.len(), 2);

        // Repair is idempotent — re-running finds nothing dangling.
        assert_eq!(repair_dangling_sessions(&db).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn clears_worker_refs_on_active_project_cards() {
        let db = Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_project(NewProject {
            id: "p1".into(),
            name: "P".into(),
            context: "".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "default".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: false,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
        })
        .await
        .unwrap();

        let new_card = |id: &str, step: &str, blocked: bool| NewCard {
            id: id.into(),
            project_id: "p1".into(),
            title: format!("Card {id}"),
            description: "".into(),
            step: step.into(),
            priority: 0,
            workflow: "default".into(),
            model: None,
            effort: None,
            blocked,
            block_reason: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
        };
        db.create_card(new_card("c1", "doing", false))
            .await
            .unwrap();
        db.create_card(new_card("c2", "done", false)).await.unwrap();
        db.create_card(new_card("c3", "doing", true)).await.unwrap();
        for id in ["c1", "c2", "c3"] {
            // worker_session_id has an FK to sessions — seed a real one.
            seed_session(&db, &format!("ws-{id}")).await;
            db.update_card(
                id,
                UpdateCard {
                    worker_session_id: Some(Some(format!("ws-{id}"))),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }

        repair_dangling_sessions(&db).await.unwrap();

        let cards = db.list_cards_by_project("p1").await.unwrap();
        let by_id = |id: &str| cards.iter().find(|c| c.id == id).unwrap();
        assert_eq!(
            by_id("c1").worker_session_id,
            None,
            "in-flight card's dead worker ref must be cleared"
        );
        assert_eq!(
            by_id("c2").worker_session_id.as_deref(),
            Some("ws-c2"),
            "done card left alone"
        );
        assert_eq!(
            by_id("c3").worker_session_id.as_deref(),
            Some("ws-c3"),
            "blocked card left alone"
        );
    }
}
