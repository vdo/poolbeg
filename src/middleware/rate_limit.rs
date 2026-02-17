use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::{
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};

/// A simple token-bucket rate limiter.
pub struct RateLimiter {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64, // tokens per second
    last_refill: Instant,
}

impl RateLimiter {
    pub fn new(max_rps: u32) -> Self {
        let max_tokens = max_rps as f64;
        Self {
            tokens: max_tokens,
            max_tokens,
            refill_rate: max_tokens,
            last_refill: Instant::now(),
        }
    }

    /// Try to consume one token. Returns true if allowed, false if rate-limited.
    pub fn try_acquire(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.max_tokens);
        self.last_refill = now;

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Axum middleware that enforces client-side rate limiting using a shared token bucket.
/// Skips the `/health` endpoint.
pub async fn client_rate_limit(
    State(limiter): State<Arc<Mutex<RateLimiter>>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    // Skip rate limiting for health checks
    if req.uri().path() == "/health" {
        return next.run(req).await;
    }

    let allowed = {
        let mut rl = limiter.lock().unwrap();
        rl.try_acquire()
    };

    if !allowed {
        metrics::counter!("poolbeg_client_rate_limited_total").increment(1);
        return rate_limit_response(req).await;
    }

    next.run(req).await
}

/// Build a JSON-RPC rate-limit error response that preserves the request `id`.
/// For batch requests, returns an array of errors with each request's `id`.
async fn rate_limit_response(req: Request<Body>) -> Response {
    let body_bytes = axum::body::to_bytes(req.into_body(), 1_048_576)
        .await
        .unwrap_or_default();

    let make_error = |id: serde_json::Value| {
        serde_json::json!({
            "jsonrpc": "2.0",
            "error": {"code": -32005, "message": "Rate limit exceeded"},
            "id": id
        })
    };

    let response_body = if body_bytes.first() == Some(&b'[') {
        // Batch request — return an error per item, preserving each id
        if let Ok(arr) = serde_json::from_slice::<Vec<serde_json::Value>>(&body_bytes) {
            let errors: Vec<_> = arr
                .iter()
                .map(|obj| make_error(obj.get("id").cloned().unwrap_or(serde_json::Value::Null)))
                .collect();
            serde_json::Value::Array(errors)
        } else {
            make_error(serde_json::Value::Null)
        }
    } else if let Ok(obj) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
        make_error(obj.get("id").cloned().unwrap_or(serde_json::Value::Null))
    } else {
        make_error(serde_json::Value::Null)
    };

    (StatusCode::TOO_MANY_REQUESTS, axum::Json(response_body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    #[test]
    fn test_rate_limiter_allows_within_limit() {
        let mut rl = RateLimiter::new(10);
        for _ in 0..10 {
            assert!(rl.try_acquire());
        }
    }

    #[test]
    fn test_rate_limiter_rejects_over_limit() {
        let mut rl = RateLimiter::new(5);
        for _ in 0..5 {
            assert!(rl.try_acquire());
        }
        assert!(!rl.try_acquire());
    }

    #[test]
    fn test_rate_limiter_refills_over_time() {
        let mut rl = RateLimiter::new(10);
        // Exhaust all tokens
        for _ in 0..10 {
            rl.try_acquire();
        }
        assert!(!rl.try_acquire());

        // Simulate time passing by backdating last_refill
        rl.last_refill = Instant::now() - std::time::Duration::from_secs(1);
        assert!(rl.try_acquire());
    }

    /// Helper: build a Request with the given JSON body.
    fn json_request(body: &str) -> Request<Body> {
        Request::builder()
            .uri("/eth")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// Helper: collect a Response body into a serde_json::Value.
    async fn response_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1_048_576)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn test_rate_limit_response_preserves_single_id() {
        let req =
            json_request(r#"{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":42}"#);
        let resp = rate_limit_response(req).await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

        let body = response_json(resp).await;
        assert_eq!(body["id"], 42);
        assert_eq!(body["error"]["code"], -32005);
    }

    #[tokio::test]
    async fn test_rate_limit_response_preserves_string_id() {
        let req = json_request(
            r#"{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":"req-abc"}"#,
        );
        let resp = rate_limit_response(req).await;
        let body = response_json(resp).await;
        assert_eq!(body["id"], "req-abc");
    }

    #[tokio::test]
    async fn test_rate_limit_response_batch_preserves_ids() {
        let req = json_request(
            r#"[{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1},{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":2}]"#,
        );
        let resp = rate_limit_response(req).await;
        let body = response_json(resp).await;

        let arr = body.as_array().expect("batch response should be array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], 1);
        assert_eq!(arr[1]["id"], 2);
        assert_eq!(arr[0]["error"]["code"], -32005);
        assert_eq!(arr[1]["error"]["code"], -32005);
    }

    #[tokio::test]
    async fn test_rate_limit_response_invalid_body_returns_null_id() {
        let req = json_request("not json at all");
        let resp = rate_limit_response(req).await;
        let body = response_json(resp).await;
        assert!(body["id"].is_null());
        assert_eq!(body["error"]["code"], -32005);
    }
}
