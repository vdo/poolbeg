use std::collections::HashSet;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Axum middleware that validates API key authentication.
/// Checks for `X-Api-Key` header or `key` query parameter.
/// Skips the `/health` endpoint.
pub async fn auth_middleware(
    State(valid_keys): State<Arc<HashSet<String>>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    // Skip auth for health checks
    if req.uri().path() == "/health" {
        return next.run(req).await;
    }

    // Check X-Api-Key header first
    let api_key = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Fall back to ?key= query parameter
    let api_key = api_key.or_else(|| {
        req.uri().query().and_then(|q| {
            q.split('&')
                .filter_map(|pair| pair.split_once('='))
                .find(|(k, _)| *k == "key")
                .map(|(_, v)| v.to_string())
        })
    });

    match api_key {
        Some(key) if valid_keys.contains(&key) => next.run(req).await,
        _ => {
            let error_body = serde_json::json!({
                "jsonrpc": "2.0",
                "error": {"code": -32001, "message": "Unauthorized: invalid or missing API key"},
                "id": null
            });

            (StatusCode::UNAUTHORIZED, axum::Json(error_body)).into_response()
        }
    }
}
