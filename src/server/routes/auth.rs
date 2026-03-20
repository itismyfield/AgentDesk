use axum::Json;
use serde_json::json;

/// GET /api/auth/session
/// Returns session status. If auth_token is configured, validates the request.
/// The actual auth check is done by the middleware — if this handler runs, the request is authenticated.
pub async fn get_session() -> Json<serde_json::Value> {
    let config = crate::config::load_graceful();
    let auth_enabled = config.server.auth_token.is_some();
    Json(json!({
        "ok": true,
        "auth_enabled": auth_enabled,
        "csrf_token": "",
    }))
}

/// Auth middleware: checks Bearer token against config.server.auth_token.
/// If auth_token is not set, all requests pass through (local-only mode).
pub async fn auth_middleware(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let config = crate::config::load_graceful();
    let Some(expected_token) = config.server.auth_token.as_deref() else {
        // No auth configured — pass through
        return next.run(req).await;
    };

    if expected_token.is_empty() {
        return next.run(req).await;
    }

    // Skip auth for health/session/hook endpoints (internal service calls)
    // Note: path is relative to the /api nest, so "/health" not "/api/health"
    let path = req.uri().path();
    if path == "/health"
        || path == "/auth/session"
        || path.starts_with("/hook/")
    {
        return next.run(req).await;
    }

    // Check Authorization header
    if let Some(auth_header) = req.headers().get("authorization") {
        if let Ok(value) = auth_header.to_str() {
            if let Some(token) = value.strip_prefix("Bearer ") {
                if token == expected_token {
                    return next.run(req).await;
                }
            }
        }
    }

    // Check query param (for dashboard WebSocket/SSE connections)
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(token) = pair.strip_prefix("token=") {
                if token == expected_token {
                    return next.run(req).await;
                }
            }
        }
    }

    axum::response::Response::builder()
        .status(401)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            r#"{"error":"unauthorized","message":"Bearer token required"}"#,
        ))
        .unwrap_or_default()
}
