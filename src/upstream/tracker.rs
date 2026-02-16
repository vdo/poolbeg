use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::cache::CacheLayer;
use crate::upstream::client::UpstreamClient;

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
    pub event_tx: broadcast::Sender<BlockEvent>,
}

impl BlockTracker {
    pub fn new(
        chain_id: u64,
        chain_name: String,
        poll_interval: Duration,
        finality_depth: u64,
    ) -> (Self, broadcast::Receiver<BlockEvent>) {
        let (event_tx, event_rx) = broadcast::channel(1024);
        (
            Self {
                chain_id,
                chain_name,
                poll_interval,
                finality_depth,
                event_tx,
            },
            event_rx,
        )
    }

    /// Run the block tracking loop. Selects the best upstream and polls for new blocks.
    pub async fn run(&self, upstreams: Arc<Vec<Arc<UpstreamClient>>>, cache: CacheLayer) {
        let mut tick = tokio::time::interval(self.poll_interval);
        let mut last_block_number: u64 = 0;
        let mut last_block_hash: String = String::new();

        info!(chain = %self.chain_name, interval_ms = %self.poll_interval.as_millis(), "block tracker started");

        loop {
            tick.tick().await;

            let upstream = match select_best_upstream(&upstreams) {
                Some(u) => u,
                None => {
                    warn!(chain = %self.chain_name, "no healthy upstreams available for block tracking");
                    continue;
                }
            };

            // Fetch latest block
            match self
                .fetch_latest_block(
                    &upstream,
                    &cache,
                    &mut last_block_number,
                    &mut last_block_hash,
                )
                .await
            {
                Ok(()) => {}
                Err(e) => {
                    warn!(chain = %self.chain_name, upstream = %upstream.id, error = %e, "failed to fetch latest block");
                }
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

        // Reorg detection: new block has same or lower number but different hash
        if number <= *last_block_number && hash != *last_block_hash && *last_block_number > 0 {
            warn!(
                chain = %self.chain_name,
                from = *last_block_number,
                to = number,
                "reorg detected"
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
        }

        debug!(chain = %self.chain_name, number, hash = %hash, "new block");

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
                debug!(chain = %self.chain_name, number, error = %e, "failed to fetch block logs");
            }
        }

        *last_block_number = number;
        *last_block_hash = hash;

        metrics::gauge!("meddler_chain_head_block",
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
}

/// Select the best healthy upstream (prefer primary, then secondary, then fallback).
fn select_best_upstream(upstreams: &[Arc<UpstreamClient>]) -> Option<Arc<UpstreamClient>> {
    // Sort by role priority, pick first healthy
    let mut sorted: Vec<_> = upstreams
        .iter()
        .filter(|u| u.is_healthy())
        .cloned()
        .collect();
    sorted.sort_by_key(|u| u.role);
    sorted.into_iter().next()
}
