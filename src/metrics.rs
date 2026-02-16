use anyhow::Result;
use axum::response::IntoResponse;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use once_cell::sync::OnceCell;

use crate::config::Config;

static PROMETHEUS_HANDLE: OnceCell<PrometheusHandle> = OnceCell::new();

pub fn init_metrics(_config: &Config) -> Result<()> {
    // Idempotent: skip if already initialized (safe for tests)
    if PROMETHEUS_HANDLE.get().is_some() {
        return Ok(());
    }

    let builder = PrometheusBuilder::new();
    let handle = match builder.install_recorder() {
        Ok(h) => h,
        Err(_) => return Ok(()), // Global recorder already set
    };
    let _ = PROMETHEUS_HANDLE.set(handle);

    // Register initial metric descriptions
    metrics::describe_counter!(
        "meddler_requests_total",
        "Total JSON-RPC requests processed"
    );
    metrics::describe_counter!("meddler_cache_hits_total", "Total cache hits");
    metrics::describe_counter!("meddler_cache_misses_total", "Total cache misses");
    metrics::describe_histogram!(
        "meddler_request_duration_seconds",
        "Request duration in seconds"
    );
    metrics::describe_histogram!(
        "meddler_upstream_request_duration_seconds",
        "Upstream request duration in seconds"
    );
    metrics::describe_gauge!(
        "meddler_upstream_healthy",
        "Whether an upstream is healthy (1=yes, 0=no)"
    );
    metrics::describe_gauge!(
        "meddler_upstream_block_height",
        "Latest block height reported by upstream"
    );
    metrics::describe_gauge!(
        "meddler_chain_head_block",
        "Current chain head block number"
    );
    metrics::describe_gauge!(
        "meddler_ws_active_connections",
        "Number of active WebSocket connections"
    );
    metrics::describe_gauge!(
        "meddler_ws_active_subscriptions",
        "Number of active WebSocket subscriptions"
    );

    Ok(())
}

pub async fn metrics_handler() -> impl IntoResponse {
    let handle = PROMETHEUS_HANDLE.get().expect("metrics not initialized");
    handle.render()
}
