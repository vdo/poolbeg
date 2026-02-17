use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::broadcast;
use tracing::{debug, info};

use crate::cache::CacheLayer;
use crate::config::{ChainConfig, UpstreamRole, UpstreamStrategy};
use crate::rpc::types::{JsonRpcRequest, JsonRpcResponse, truncate_json};
use crate::server::AppState;
use crate::upstream::client::UpstreamClient;
use crate::upstream::health;
use crate::upstream::strategy;
use crate::upstream::tracker::{BlockEvent, BlockTracker};

/// Manages upstreams and block tracking for a single chain.
pub struct ChainManager {
    pub chain_name: String,
    pub chain_id: u64,
    pub upstreams: Arc<Vec<Arc<UpstreamClient>>>,
    pub tracker: Arc<BlockTracker>,
    pub event_rx: broadcast::Receiver<BlockEvent>,
    strategy: UpstreamStrategy,
    round_robin: AtomicUsize,
    cache: CacheLayer,
    debug_upstream: bool,
    /// Number of active WebSocket connections on this chain.
    pub ws_connections: AtomicUsize,
    /// Number of active WebSocket subscriptions on this chain.
    pub ws_subscriptions: AtomicUsize,
}

impl ChainManager {
    pub async fn new(
        config: &ChainConfig,
        cache: CacheLayer,
        debug_upstream: bool,
    ) -> Result<Self> {
        let mut upstreams = Vec::new();
        for upstream_config in &config.upstreams {
            let client = UpstreamClient::new(
                upstream_config,
                config.disabled_retry_interval,
                config.name.clone(),
            )?;
            upstreams.push(Arc::new(client));
        }
        let upstreams = Arc::new(upstreams);

        let (tracker, event_rx) = BlockTracker::new(
            config.chain_id,
            config.name.clone(),
            config.expected_block_time,
            config.finality_depth,
            config.strategy,
        );

        Ok(Self {
            chain_name: config.name.clone(),
            chain_id: config.chain_id,
            upstreams,
            tracker: Arc::new(tracker),
            event_rx,
            strategy: config.strategy,
            round_robin: AtomicUsize::new(0),
            cache,
            debug_upstream,
            ws_connections: AtomicUsize::new(0),
            ws_subscriptions: AtomicUsize::new(0),
        })
    }

    /// Start health check loops and the block tracker.
    pub fn start_tracker(&self, chain_idx: usize, state: Arc<AppState>) {
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

        // Start upstream status summary loop
        {
            let upstreams = self.upstreams.clone();
            let chain_name = self.chain_name.clone();
            tokio::spawn(health::upstream_status_loop(
                upstreams,
                chain_name,
                state.clone(),
                chain_idx,
            ));
        }

        // Start block tracker
        let tracker = self.tracker.clone();
        let upstreams = self.upstreams.clone();
        let cache = self.cache.clone();
        tokio::spawn(async move {
            tracker.run(upstreams, cache).await;
        });

        info!(
            "[{}] started health checks and block tracker",
            self.chain_name
        );
    }

    /// Subscribe to block events.
    pub fn subscribe_events(&self) -> broadcast::Receiver<BlockEvent> {
        self.tracker.event_tx.subscribe()
    }

    /// Forward a JSON-RPC request to the best available upstream.
    /// Uses role-based tier selection with the configured strategy within tiers.
    /// Falls back through all upstreams in the tier on failure.
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
                .cloned()
                .collect();

            if tier_upstreams.is_empty() {
                continue;
            }

            // Pick the preferred upstream via strategy
            let preferred = strategy::select(&tier_upstreams, self.strategy, &self.round_robin);

            // Build an ordered attempt list: preferred first, then the rest
            let mut attempt_order: Vec<Arc<UpstreamClient>> =
                Vec::with_capacity(tier_upstreams.len());
            if let Some(ref pref) = preferred {
                attempt_order.push(pref.clone());
            }
            for u in &tier_upstreams {
                if preferred.as_ref().is_none_or(|p| !Arc::ptr_eq(p, u)) {
                    attempt_order.push(u.clone());
                }
            }

            for upstream in &attempt_order {
                // Check per-upstream rate limit
                if !upstream.try_acquire_rate_limit() {
                    metrics::counter!("poolbeg_upstream_rate_limited_total",
                        "chain" => self.chain_name.clone(),
                        "upstream" => upstream.id.clone()
                    )
                    .increment(1);
                    continue;
                }

                if self.debug_upstream {
                    debug!(
                        upstream = %upstream.id,
                        method = %req.method,
                        id = %req.id,
                        params = %truncate_json(&req.params, 512),
                        "[{}] \u{2192} upstream request", self.chain_name
                    );
                }

                let start = std::time::Instant::now();
                match upstream.send_request(req).await {
                    Ok(resp) => {
                        if self.debug_upstream {
                            let elapsed = start.elapsed();
                            if let Some(ref err) = resp.error {
                                debug!(
                                    upstream = %upstream.id,
                                    method = %req.method,
                                    id = %resp.id,
                                    elapsed_ms = %elapsed.as_millis(),
                                    error_code = err.code,
                                    error_msg = %err.message,
                                    "[{}] \u{2190} upstream error", self.chain_name
                                );
                            } else {
                                let result_str = resp
                                    .result
                                    .as_ref()
                                    .map(|r| truncate_json(r, 512))
                                    .unwrap_or_else(|| "null".to_string());
                                debug!(
                                    upstream = %upstream.id,
                                    method = %req.method,
                                    id = %resp.id,
                                    elapsed_ms = %elapsed.as_millis(),
                                    result = %result_str,
                                    "[{}] \u{2190} upstream response", self.chain_name
                                );
                            }
                        }

                        upstream.record_success();
                        metrics::counter!("poolbeg_requests_total",
                            "chain" => self.chain_name.clone(),
                            "method" => req.method.clone(),
                            "status" => "ok",
                            "cache_hit" => "false"
                        )
                        .increment(1);
                        return Ok(resp);
                    }
                    Err(e) => {
                        if self.debug_upstream {
                            let elapsed = start.elapsed();
                            debug!(
                                upstream = %upstream.id,
                                method = %req.method,
                                id = %req.id,
                                elapsed_ms = %elapsed.as_millis(),
                                error = %e,
                                "[{}] \u{2190} upstream transport error", self.chain_name
                            );
                        }

                        upstream.record_failure();
                        tracing::warn!(
                            upstream = %upstream.id,
                            error = %e,
                            "[{}] upstream request failed, trying next", self.chain_name
                        );
                        continue;
                    }
                }
            }
        }

        anyhow::bail!("all upstreams failed for chain {}", self.chain_name)
    }
}
