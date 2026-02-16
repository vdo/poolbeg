use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::upstream::client::UpstreamClient;

/// Tick interval for the health-check polling loop. The actual per-upstream
/// check cadence is governed by the backoff state on `UpstreamClient`.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Run periodic health checks on an upstream with exponential backoff.
/// After [`MAX_CONSECUTIVE_FAILURES`](crate::upstream::client) consecutive
/// failures the upstream is disabled and scheduled for retry after the
/// configured `disabled_retry_interval`.
pub async fn health_check_loop(
    upstream: Arc<UpstreamClient>,
    chain_name: String,
    _interval: Duration,
) {
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    loop {
        tick.tick().await;

        // Respect exponential backoff / disabled retry — skip until due.
        if !upstream.is_due_for_check() {
            continue;
        }

        let was_disabled = upstream.is_disabled();
        let healthy = check_upstream(&upstream, &chain_name).await;
        let was_healthy = upstream.is_healthy();

        // Don't override disabled state if the upstream has too many HTTP errors —
        // even if the lightweight health check passes, real traffic is failing.
        if healthy && upstream.is_disabled() && upstream.has_too_many_http_errors() {
            debug!(
                upstream = %upstream.id,
                "[{chain_name}] health check passed but upstream still has too many HTTP errors, staying disabled"
            );
            upstream.record_success();
            metrics::gauge!("meddler_upstream_healthy",
                "chain" => chain_name.clone(),
                "upstream_id" => upstream.id.clone()
            )
            .set(-1.0);
            continue;
        }

        upstream.set_healthy(healthy);
        metrics::gauge!("meddler_upstream_healthy",
            "chain" => chain_name.clone(),
            "upstream_id" => upstream.id.clone()
        )
        .set(if upstream.is_disabled() { -1.0 } else if healthy { 1.0 } else { 0.0 });

        if healthy {
            if was_disabled {
                info!(upstream = %upstream.id, "[{chain_name}] disabled upstream recovered after retry");
            } else if !was_healthy {
                info!(upstream = %upstream.id, "[{chain_name}] upstream recovered");
            }
            upstream.record_success();
        } else {
            upstream.record_failure();
            let failures = upstream.consecutive_failures();

            if upstream.is_disabled() && !was_disabled {
                warn!(
                    upstream = %upstream.id,
                    consecutive_failures = failures,
                    "[{chain_name}] upstream disabled after repeated failures, will retry later"
                );
            } else if upstream.is_disabled() && was_disabled {
                debug!(
                    upstream = %upstream.id,
                    "[{chain_name}] disabled upstream retry failed, scheduling next retry"
                );
            } else if was_healthy {
                warn!(upstream = %upstream.id, "[{chain_name}] upstream became unhealthy");
            } else {
                debug!(
                    upstream = %upstream.id,
                    consecutive_failures = failures,
                    "[{chain_name}] upstream still unhealthy, backing off"
                );
            }
        }
    }
}

/// Interval between upstream status summary logs.
const STATUS_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// Periodically log a summary of upstream health for a chain,
/// including WS client and subscription counts.
pub async fn upstream_status_loop(
    upstreams: Arc<Vec<Arc<UpstreamClient>>>,
    chain_name: String,
    state: Arc<crate::server::AppState>,
    chain_idx: usize,
) {
    let mut tick = tokio::time::interval(STATUS_LOG_INTERVAL);
    loop {
        tick.tick().await;

        let total = upstreams.len();
        let mut ok = 0u32;
        let mut unhealthy = 0u32;
        let mut disabled = 0u32;

        for u in upstreams.iter() {
            if u.is_disabled() {
                disabled += 1;
            } else if u.is_healthy() {
                ok += 1;
            } else {
                unhealthy += 1;
            }
        }

        let cm = &state.chain_managers[chain_idx];
        let ws_clients = cm.ws_connections.load(std::sync::atomic::Ordering::Relaxed);
        let ws_subs = cm.ws_subscriptions.load(std::sync::atomic::Ordering::Relaxed);

        metrics::gauge!("meddler_upstream_ok", "chain" => chain_name.clone()).set(ok as f64);
        metrics::gauge!("meddler_upstream_unhealthy", "chain" => chain_name.clone()).set(unhealthy as f64);
        metrics::gauge!("meddler_upstream_disabled", "chain" => chain_name.clone()).set(disabled as f64);

        info!(
            ok,
            unhealthy,
            disabled,
            total,
            ws_clients,
            ws_subs,
            "[{chain_name}] upstreams {ok}/{unhealthy}/{disabled}/{total} (ok/nok/disabled/total)"
        );
    }
}

async fn check_upstream(upstream: &UpstreamClient, chain_name: &str) -> bool {
    // Call eth_blockNumber as a health check
    match upstream
        .call_method("eth_blockNumber", serde_json::json!([]))
        .await
    {
        Ok(resp) => {
            if let Some(result) = &resp.result
                && let Some(hex) = result.as_str()
                && let Ok(height) = u64::from_str_radix(hex.trim_start_matches("0x"), 16)
            {
                upstream.set_block_height(height);
                metrics::gauge!("meddler_upstream_block_height",
                    "upstream_id" => upstream.id.clone()
                )
                .set(height as f64);
                debug!(upstream = %upstream.id, height, "[{chain_name}] health check ok");
                return true;
            }
            warn!(upstream = %upstream.id, "[{chain_name}] health check: unexpected response format");
            false
        }
        Err(e) => {
            warn!(upstream = %upstream.id, error = %e, "[{chain_name}] health check failed");
            false
        }
    }
}
