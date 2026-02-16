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

        let error_body = serde_json::json!({
            "jsonrpc": "2.0",
            "error": {"code": -32005, "message": "Rate limit exceeded"},
            "id": null
        });

        return (StatusCode::TOO_MANY_REQUESTS, axum::Json(error_body)).into_response();
    }

    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
