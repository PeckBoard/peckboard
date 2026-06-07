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

    headers.insert(
        "Content-Security-Policy",
        HeaderValue::from_static(
            "default-src 'self'; \
             script-src 'self'; \
             style-src 'self'; \
             style-src-attr 'unsafe-inline'; \
             img-src 'self' data: blob:; \
             connect-src 'self'; \
             frame-ancestors 'none'; \
             object-src 'none'"
        ),
    );

    headers.insert(
        "X-Content-Type-Options",
        HeaderValue::from_static("nosniff"),
    );

    headers.insert(
        "X-Frame-Options",
        HeaderValue::from_static("DENY"),
    );

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

    // Also allow if path is /mcp (checked separately for loopback + token auth)
    let path = request.uri().path();
    if path == "/mcp" {
        return next.run(request).await;
    }

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
                        "reason": "peckboard-crash",
                    }),
                )
                .await?;
                repaired += 1;
                tracing::warn!(
                    "Repaired dangling agent-start for session {}",
                    session.id
                );
            }
        }
    }

    if repaired > 0 {
        tracing::info!("Repaired {repaired} dangling session(s)");
    }

    Ok(repaired)
}
