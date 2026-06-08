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
