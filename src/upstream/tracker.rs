use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::cache::CacheLayer;
use crate::config::UpstreamStrategy;
use crate::upstream::client::UpstreamClient;
use crate::upstream::strategy;

/// Events emitted by the block tracker.
#[derive(Debug, Clone)]
pub enum BlockEvent {
    /// New block header received.
    NewBlock {
        chain_id: u64,
        number: u64,
        hash: String,
        header: Value,
    },
    /// New logs for a block.
    NewLogs {
        chain_id: u64,
        block_number: u64,
        logs: Value,
    },
    /// Reorg detected: chain reorganized from `from_block` to `to_block`.
    Reorg {
        chain_id: u64,
        from_block: u64,
        to_block: u64,
    },
}

/// Block Tracker polls upstreams for new blocks and caches them.
pub struct BlockTracker {
    pub chain_id: u64,
    pub chain_name: String,
    pub poll_interval: Duration,
    pub finality_depth: u64,
    pub strategy: UpstreamStrategy,
    pub event_tx: broadcast::Sender<BlockEvent>,
    round_robin: AtomicUsize,
}

impl BlockTracker {
    pub fn new(
        chain_id: u64,
        chain_name: String,
        poll_interval: Duration,
        finality_depth: u64,
        strategy: UpstreamStrategy,
    ) -> (Self, broadcast::Receiver<BlockEvent>) {
        let (event_tx, event_rx) = broadcast::channel(1024);
        (
            Self {
                chain_id,
                chain_name,
                poll_interval,
                finality_depth,
                strategy,
                event_tx,
                round_robin: AtomicUsize::new(0),
            },
            event_rx,
        )
    }

    /// Run the block tracking loop. Selects the best upstream and polls for new blocks.
    pub async fn run(&self, upstreams: Arc<Vec<Arc<UpstreamClient>>>, cache: CacheLayer) {
        let mut tick = tokio::time::interval(self.poll_interval);
        let mut last_block_number: u64 = 0;
        let mut last_block_hash: String = String::new();

        info!(interval_ms = %self.poll_interval.as_millis(), "[{}] block tracker started", self.chain_name);

        loop {
            tick.tick().await;

            let upstream = match self.select_best_upstream(&upstreams) {
                Some(u) => u,
                None => {
                    warn!(
                        "[{}] no healthy upstreams available for block tracking",
                        self.chain_name
                    );
                    continue;
                }
            };

            // Check rate limit before polling
            if !upstream.try_acquire_rate_limit() {
                debug!(upstream = %upstream.id, "[{}] skipping block poll, upstream rate limited", self.chain_name);
                continue;
            }

            // Fetch latest block
            if let Err(e) = self
                .fetch_latest_block(
                    &upstream,
                    &cache,
                    &mut last_block_number,
                    &mut last_block_hash,
                )
                .await
            {
                warn!(upstream = %upstream.id, error = %e, "[{}] failed to fetch latest block", self.chain_name);
            }
        }
    }

    async fn fetch_latest_block(
        &self,
        upstream: &UpstreamClient,
        cache: &CacheLayer,
        last_block_number: &mut u64,
        last_block_hash: &mut String,
    ) -> anyhow::Result<()> {
        let resp = upstream
            .call_method("eth_getBlockByNumber", serde_json::json!(["latest", false]))
            .await?;

        let block = match resp.result {
            Some(ref v) if !v.is_null() => v,
            _ => return Ok(()),
        };

        let number_hex = block["number"].as_str().unwrap_or("0x0");
        let number = u64::from_str_radix(number_hex.trim_start_matches("0x"), 16).unwrap_or(0);
        let hash = block["hash"].as_str().unwrap_or("").to_string();

        if number == 0 || hash.is_empty() {
            return Ok(());
        }

        // Same block, skip
        if number == *last_block_number && hash == *last_block_hash {
            return Ok(());
        }

        // Block with a lower number than what we've already seen — this is a
        // lagging upstream, not a reorg.  With multiple upstreams (especially on
        // fast-block chains like Arbitrum) slight height differences are normal.
        if number < *last_block_number && *last_block_number > 0 {
            debug!(
                last = *last_block_number,
                received = number,
                "[{}] ignoring block from lagging upstream",
                self.chain_name
            );
            return Ok(());
        }

        // Reorg detection: same block number but a different hash means the
        // chain tip was replaced.  This is the only reliable signal when
        // polling across multiple upstreams.
        if number == *last_block_number && hash != *last_block_hash && *last_block_number > 0 {
            warn!(
                number,
                old_hash = %last_block_hash,
                new_hash = %hash,
                "[{}] reorg detected (same height, different hash)", self.chain_name
            );
            let _ = self.event_tx.send(BlockEvent::Reorg {
                chain_id: self.chain_id,
                from_block: *last_block_number,
                to_block: number,
            });
            // Invalidate head cache on reorg
            cache
                .invalidate_head_cache(self.chain_id, &self.chain_name)
                .await;
            // Update hash so we don't re-fire on the next tick
            *last_block_hash = hash;
            return Ok(());
        }

        debug!(number, hash = %hash, "[{}] new block", self.chain_name);

        // Cache the block by number and hash
        cache.cache_block(self.chain_id, number, &hash, block).await;

        // Emit NewBlock event
        let _ = self.event_tx.send(BlockEvent::NewBlock {
            chain_id: self.chain_id,
            number,
            hash: hash.clone(),
            header: block.clone(),
        });

        // Fetch and cache logs for this block
        match self.fetch_block_logs(upstream, cache, number).await {
            Ok(()) => {}
            Err(e) => {
                debug!(number, error = %e, "[{}] failed to fetch block logs", self.chain_name);
            }
        }

        *last_block_number = number;
        *last_block_hash = hash;

        metrics::gauge!("poolbeg_chain_head_block",
            "chain" => self.chain_name.clone()
        )
        .set(number as f64);

        Ok(())
    }

    async fn fetch_block_logs(
        &self,
        upstream: &UpstreamClient,
        cache: &CacheLayer,
        block_number: u64,
    ) -> anyhow::Result<()> {
        let block_hex = format!("0x{:x}", block_number);
        let resp = upstream
            .call_method(
                "eth_getLogs",
                serde_json::json!([{"fromBlock": block_hex, "toBlock": block_hex}]),
            )
            .await?;

        if let Some(logs) = &resp.result {
            cache.cache_logs(self.chain_id, block_number, logs).await;

            let _ = self.event_tx.send(BlockEvent::NewLogs {
                chain_id: self.chain_id,
                block_number,
                logs: logs.clone(),
            });
        }

        Ok(())
    }

    /// Select the best healthy upstream using the configured strategy.
    /// Prefers higher-priority roles (primary > secondary > fallback),
    /// then applies the strategy within the best available tier.
    fn select_best_upstream(
        &self,
        upstreams: &[Arc<UpstreamClient>],
    ) -> Option<Arc<UpstreamClient>> {
        use crate::config::UpstreamRole;

        for role in &[
            UpstreamRole::Primary,
            UpstreamRole::Secondary,
            UpstreamRole::Fallback,
        ] {
            let tier: Vec<_> = upstreams
                .iter()
                .filter(|u| u.role == *role && u.is_healthy() && !u.is_disabled())
                .cloned()
                .collect();
            if !tier.is_empty() {
                return strategy::select(&tier, self.strategy, &self.round_robin);
            }
        }
        None
    }
}
