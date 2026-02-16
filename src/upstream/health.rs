use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::upstream::client::UpstreamClient;

/// Run periodic health checks on an upstream.
pub async fn health_check_loop(
    upstream: Arc<UpstreamClient>,
    chain_name: String,
    interval: Duration,
) {
    let mut tick = tokio::time::interval(interval);
    loop {
        tick.tick().await;
        let healthy = check_upstream(&upstream).await;
        let was_healthy = upstream.is_healthy();

        upstream.set_healthy(healthy);
        metrics::gauge!("meddler_upstream_healthy",
            "chain" => chain_name.clone(),
            "upstream_id" => upstream.id.clone()
        )
        .set(if healthy { 1.0 } else { 0.0 });

        if was_healthy && !healthy {
            warn!(chain = %chain_name, upstream = %upstream.id, "upstream became unhealthy");
        } else if !was_healthy && healthy {
            info!(chain = %chain_name, upstream = %upstream.id, "upstream recovered");
        }
    }
}

async fn check_upstream(upstream: &UpstreamClient) -> bool {
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
                debug!(upstream = %upstream.id, height, "health check ok");
                return true;
            }
            warn!(upstream = %upstream.id, "health check: unexpected response format");
            false
        }
        Err(e) => {
            warn!(upstream = %upstream.id, error = %e, "health check failed");
            false
        }
    }
}
