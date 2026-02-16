use anyhow::Result;
use reqwest::Client;
use reqwest::header;
use serde_json::Value;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tracing::debug;

use crate::config::{UpstreamConfig, UpstreamRole};
use crate::middleware::rate_limit::RateLimiter;
use crate::rpc::types::{JsonRpcRequest, JsonRpcResponse};

const USER_AGENT: &str = concat!("poolbeg/", env!("CARGO_PKG_VERSION"));

/// Base health check interval before backoff kicks in.
const BASE_BACKOFF: Duration = Duration::from_secs(10);
/// Maximum backoff interval between health check retries.
const MAX_BACKOFF: Duration = Duration::from_secs(600);
/// Number of consecutive failures before the upstream is permanently disabled.
const MAX_CONSECUTIVE_FAILURES: u32 = 6;
/// Number of HTTP errors (4xx, 5xx, unreachable) within ERROR_WINDOW before disable.
const MAX_HTTP_ERRORS: usize = 5;
/// Time window for counting HTTP errors.
const HTTP_ERROR_WINDOW: Duration = Duration::from_secs(3600);

/// An individual upstream RPC node.
pub struct UpstreamClient {
    pub id: String,
    pub role: UpstreamRole,
    pub http_url: String,
    pub ws_url: Option<String>,
    pub max_rps: u32,
    chain_name: String,
    client: Client,
    healthy: AtomicBool,
    /// Disabled after max consecutive failures. Will be retried after
    /// `disabled_retry_interval`.
    disabled: AtomicBool,
    consecutive_failures: AtomicU32,
    /// Next time a health check should be attempted. Protected by Mutex for
    /// interior mutability of `Instant` (which is not atomic).
    next_check_at: Mutex<Instant>,
    /// How long to wait before retrying a disabled upstream.
    disabled_retry_interval: Duration,
    block_height: AtomicU64,
    /// Exponential moving average of request latency in microseconds.
    latency_us: AtomicU64,
    rate_limiter: Mutex<RateLimiter>,
    /// Timestamps of recent HTTP errors — 4xx, 5xx, unreachable (pruned to HTTP_ERROR_WINDOW).
    http_error_times: Mutex<Vec<Instant>>,
}

impl UpstreamClient {
    pub fn new(
        config: &UpstreamConfig,
        disabled_retry_interval: Duration,
        chain_name: String,
    ) -> Result<Self> {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());

        let client = Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(32)
            .build()?;

        Ok(Self {
            id: config.id.clone(),
            role: config.role,
            http_url: config.http_url.clone(),
            ws_url: config.ws_url.clone(),
            max_rps: config.max_rps,
            chain_name,
            client,
            healthy: AtomicBool::new(true),
            disabled: AtomicBool::new(false),
            consecutive_failures: AtomicU32::new(0),
            next_check_at: Mutex::new(Instant::now()),
            disabled_retry_interval,
            block_height: AtomicU64::new(0),
            latency_us: AtomicU64::new(u64::MAX),
            rate_limiter: Mutex::new(RateLimiter::new(config.max_rps)),
            http_error_times: Mutex::new(Vec::new()),
        })
    }

    pub fn is_healthy(&self) -> bool {
        !self.disabled.load(Ordering::Relaxed) && self.healthy.load(Ordering::Relaxed)
    }

    pub fn set_healthy(&self, healthy: bool) {
        self.healthy.store(healthy, Ordering::Relaxed);
    }

    /// Whether the upstream has been permanently disabled due to repeated failures.
    pub fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Relaxed)
    }

    /// Returns true if the upstream is due for a health check (backoff has elapsed).
    /// Disabled upstreams become due again after their retry interval.
    pub fn is_due_for_check(&self) -> bool {
        let next = *self.next_check_at.lock().unwrap();
        Instant::now() >= next
    }

    /// Returns true if the HTTP error window is at or above the disable threshold.
    pub fn has_too_many_http_errors(&self) -> bool {
        let now = Instant::now();
        let mut times = self.http_error_times.lock().unwrap();
        times.retain(|t| now.duration_since(*t) < HTTP_ERROR_WINDOW);
        times.len() >= MAX_HTTP_ERRORS
    }

    /// Record a successful health check: reset failure count and backoff.
    /// Will NOT re-enable the upstream if it was disabled due to HTTP errors
    /// (the 1h window must expire first).  This prevents flip-flop where a
    /// lightweight health check passes while real traffic keeps getting 429s.
    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        if self.disabled.load(Ordering::Relaxed) {
            if self.has_too_many_http_errors() {
                // Still over the HTTP error threshold — stay disabled,
                // schedule another retry later.
                *self.next_check_at.lock().unwrap() = Instant::now() + self.disabled_retry_interval;
                return;
            }
            // HTTP errors have aged out — safe to re-enable
            self.http_error_times.lock().unwrap().clear();
        }
        self.disabled.store(false, Ordering::Relaxed);
        *self.next_check_at.lock().unwrap() = Instant::now();
    }

    /// Record a failed health check: increment failure count, compute next
    /// backoff delay, and disable the upstream if max failures is reached.
    /// Disabled upstreams are scheduled for retry after `disabled_retry_interval`.
    pub fn record_failure(&self) {
        let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if failures >= MAX_CONSECUTIVE_FAILURES {
            self.disabled.store(true, Ordering::Relaxed);
            self.healthy.store(false, Ordering::Relaxed);
            // Schedule retry after the configured interval
            *self.next_check_at.lock().unwrap() = Instant::now() + self.disabled_retry_interval;
            return;
        }
        // Exponential backoff: base * 2^(failures-1), capped at MAX_BACKOFF
        let backoff = BASE_BACKOFF
            .saturating_mul(1u32.wrapping_shl(failures.saturating_sub(1)))
            .min(MAX_BACKOFF);
        *self.next_check_at.lock().unwrap() = Instant::now() + backoff;
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    /// Get the current exponential moving average latency in microseconds.
    /// Returns `u64::MAX` if no measurement has been recorded yet.
    pub fn latency_us(&self) -> u64 {
        self.latency_us.load(Ordering::Relaxed)
    }

    /// Update latency EMA. Uses α=0.3 so recent measurements weigh more.
    fn update_latency(&self, elapsed: Duration) {
        let new_us = elapsed.as_micros() as u64;
        let prev = self.latency_us.load(Ordering::Relaxed);
        let ema = if prev == u64::MAX {
            new_us
        } else {
            // EMA: α * new + (1 - α) * old, with α = 0.3 (using integer math: 3/10)
            (new_us * 3 + prev * 7) / 10
        };
        self.latency_us.store(ema, Ordering::Relaxed);
    }

    /// Expose `update_latency` for tests.
    #[cfg(test)]
    pub fn update_latency_for_test(&self, elapsed: Duration) {
        self.update_latency(elapsed);
    }

    pub fn block_height(&self) -> u64 {
        self.block_height.load(Ordering::Relaxed)
    }

    pub fn set_block_height(&self, height: u64) {
        self.block_height.store(height, Ordering::Relaxed);
    }

    /// Try to acquire a rate limit token. Returns true if allowed.
    pub fn try_acquire_rate_limit(&self) -> bool {
        let mut rl = self.rate_limiter.lock().unwrap();
        rl.try_acquire()
    }

    /// Record an HTTP-level error (4xx, 5xx, connection failure).
    /// If the count within the 1h window reaches MAX_HTTP_ERRORS the upstream
    /// is disabled.
    fn record_http_error(&self) {
        let now = Instant::now();
        let mut times = self.http_error_times.lock().unwrap();
        times.push(now);
        times.retain(|t| now.duration_since(*t) < HTTP_ERROR_WINDOW);
        if times.len() >= MAX_HTTP_ERRORS {
            tracing::warn!(
                upstream = %self.id,
                errors = times.len(),
                window = "1h",
                "[{}] upstream disabled after too many HTTP errors", self.chain_name
            );
            drop(times);
            self.disabled.store(true, Ordering::Relaxed);
            self.healthy.store(false, Ordering::Relaxed);
            *self.next_check_at.lock().unwrap() = Instant::now() + self.disabled_retry_interval;
        }
    }

    /// Send a JSON-RPC request to this upstream and return the response.
    pub async fn send_request(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse> {
        let body = serde_json::to_vec(req)?;

        let start = std::time::Instant::now();
        let response = match self.client.post(&self.http_url).body(body).send().await {
            Ok(r) => r,
            Err(e) => {
                self.record_http_error();
                return Err(e.into());
            }
        };

        let elapsed = start.elapsed();
        debug!(upstream = %self.id, method = %req.method, elapsed_ms = %elapsed.as_millis(), "upstream response");

        metrics::histogram!("poolbeg_upstream_request_duration_seconds", "upstream" => self.id.clone())
            .record(elapsed.as_secs_f64());

        self.update_latency(elapsed);

        let status = response.status();
        if !status.is_success() {
            if status.is_client_error() || status.is_server_error() {
                self.record_http_error();
            }
            anyhow::bail!("upstream {} returned HTTP {}", self.id, status,);
        }

        let resp: JsonRpcResponse = response.json().await?;
        Ok(resp)
    }

    /// Send a raw JSON-RPC call (convenience for internal calls like eth_blockNumber).
    pub async fn call_method(&self, method: &str, params: Value) -> Result<JsonRpcResponse> {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
            id: Value::Number(1.into()),
        };
        self.send_request(&req).await
    }
}
