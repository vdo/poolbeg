use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::broadcast;
use tracing::info;

use crate::cache::CacheLayer;
use crate::config::{ChainConfig, UpstreamRole};
use crate::rpc::types::{JsonRpcRequest, JsonRpcResponse};
use crate::server::AppState;
use crate::upstream::client::UpstreamClient;
use crate::upstream::health;
use crate::upstream::tracker::{BlockEvent, BlockTracker};

/// Manages upstreams and block tracking for a single chain.
pub struct ChainManager {
    pub chain_name: String,
    pub chain_id: u64,
    pub upstreams: Arc<Vec<Arc<UpstreamClient>>>,
    pub tracker: Arc<BlockTracker>,
    pub event_rx: broadcast::Receiver<BlockEvent>,
    round_robin: AtomicUsize,
    cache: CacheLayer,
}

impl ChainManager {
    pub async fn new(config: &ChainConfig, cache: CacheLayer) -> Result<Self> {
        let mut upstreams = Vec::new();
        for upstream_config in &config.upstreams {
            let client = UpstreamClient::new(upstream_config)?;
            upstreams.push(Arc::new(client));
        }
        let upstreams = Arc::new(upstreams);

        let (tracker, event_rx) = BlockTracker::new(
            config.chain_id,
            config.name.clone(),
            config.expected_block_time,
            config.finality_depth,
        );

        Ok(Self {
            chain_name: config.name.clone(),
            chain_id: config.chain_id,
            upstreams,
            tracker: Arc::new(tracker),
            event_rx,
            round_robin: AtomicUsize::new(0),
            cache,
        })
    }

    /// Start health check loops and the block tracker.
    pub fn start_tracker(&self, _chain_idx: usize, _state: Arc<AppState>) {
        // Start health checks for each upstream
        let health_interval = Duration::from_secs(10);
        for upstream in self.upstreams.iter() {
            let upstream = upstream.clone();
            let chain_name = self.chain_name.clone();
            tokio::spawn(health::health_check_loop(
                upstream,
                chain_name,
                health_interval,
            ));
        }

        // Start block tracker
        let tracker = self.tracker.clone();
        let upstreams = self.upstreams.clone();
        let cache = self.cache.clone();
        tokio::spawn(async move {
            tracker.run(upstreams, cache).await;
        });

        info!(chain = %self.chain_name, "started health checks and block tracker");
    }

    /// Subscribe to block events.
    pub fn subscribe_events(&self) -> broadcast::Receiver<BlockEvent> {
        self.tracker.event_tx.subscribe()
    }

    /// Forward a JSON-RPC request to the best available upstream.
    /// Uses role-based tier selection with round-robin within tiers.
    /// Checks per-upstream rate limits and skips rate-limited upstreams.
    pub async fn forward_request(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse> {
        // Try tiers in order: primary, secondary, fallback
        for role in &[
            UpstreamRole::Primary,
            UpstreamRole::Secondary,
            UpstreamRole::Fallback,
        ] {
            let tier_upstreams: Vec<_> = self
                .upstreams
                .iter()
                .filter(|u| u.role == *role && u.is_healthy())
                .collect();

            if tier_upstreams.is_empty() {
                continue;
            }

            let start_idx = self.round_robin.fetch_add(1, Ordering::Relaxed);

            // Iterate all upstreams in the tier (starting from round-robin offset)
            for i in 0..tier_upstreams.len() {
                let idx = (start_idx + i) % tier_upstreams.len();
                let upstream = &tier_upstreams[idx];

                // Check per-upstream rate limit
                if !upstream.try_acquire_rate_limit() {
                    metrics::counter!("meddler_upstream_rate_limited_total",
                        "chain" => self.chain_name.clone(),
                        "upstream" => upstream.id.clone()
                    )
                    .increment(1);
                    continue;
                }

                match upstream.send_request(req).await {
                    Ok(resp) => {
                        metrics::counter!("meddler_requests_total",
                            "chain" => self.chain_name.clone(),
                            "method" => req.method.clone(),
                            "status" => "ok",
                            "cache_hit" => "false"
                        )
                        .increment(1);
                        return Ok(resp);
                    }
                    Err(e) => {
                        tracing::warn!(
                            chain = %self.chain_name,
                            upstream = %upstream.id,
                            error = %e,
                            "upstream request failed, trying next"
                        );
                        // Mark unhealthy and try next
                        upstream.set_healthy(false);
                        continue;
                    }
                }
            }
        }

        anyhow::bail!("all upstreams failed for chain {}", self.chain_name)
    }
}
