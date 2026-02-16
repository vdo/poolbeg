pub mod keys;
pub mod policy;
pub mod redis;

use anyhow::Result;
use serde_json::Value;
use tracing::debug;

use crate::config::CacheConfig;
use crate::rpc::types::JsonRpcRequest;
use crate::rpc::types::JsonRpcResponse;

/// The cache layer wrapping Redis and cache policies.
#[derive(Clone)]
pub struct CacheLayer {
    redis: redis::RedisClient,
    config: CacheConfig,
}

impl CacheLayer {
    pub async fn new(config: &CacheConfig) -> Result<Self> {
        let redis_client =
            redis::RedisClient::new(&config.redis.url, config.redis.pool_size).await?;
        Ok(Self {
            redis: redis_client,
            config: config.clone(),
        })
    }

    /// Try to get a cached response for a request.
    pub async fn get(&self, chain_id: u64, req: &JsonRpcRequest) -> Option<String> {
        let key = keys::cache_key(chain_id, req);
        self.redis.get(&key).await
    }

    /// Cache a response with appropriate TTL based on policy.
    pub async fn set(
        &self,
        chain_id: u64,
        req: &JsonRpcRequest,
        resp: &JsonRpcResponse,
        is_unfinalized: bool,
        chain_name: &str,
    ) {
        let ttl = match policy::resolve_ttl(&self.config, &req.method, is_unfinalized) {
            Some(ttl) => ttl,
            None => return, // Policy says don't cache
        };

        let key = keys::cache_key(chain_id, req);
        let value = match serde_json::to_string(resp) {
            Ok(v) => v,
            Err(_) => return,
        };

        self.redis.set(&key, &value, ttl).await;

        // Track unfinalized entries in head cache set for later invalidation
        if is_unfinalized {
            let set_key = keys::head_cache_set_key(chain_id);
            self.redis.sadd(&set_key, &key).await;
        }

        debug!(chain = %chain_name, method = %req.method, ttl_ms = ttl.as_millis(), "cached response");
    }

    /// Cache a block by number and hash (called by block tracker).
    pub async fn cache_block(&self, chain_id: u64, number: u64, hash: &str, block: &Value) {
        let value = serde_json::to_string(block).unwrap_or_default();

        // Cache by number (short TTL since "latest" changes)
        let num_key = keys::block_by_number_key(chain_id, number);
        self.redis
            .set(&num_key, &value, std::time::Duration::from_secs(60))
            .await;

        // Cache by hash (longer TTL, block hash is immutable unless reorg)
        let hash_key = keys::block_by_hash_key(chain_id, hash);
        self.redis
            .set(&hash_key, &value, std::time::Duration::from_secs(3600))
            .await;

        // Track in head cache for potential reorg invalidation
        let set_key = keys::head_cache_set_key(chain_id);
        self.redis.sadd(&set_key, &num_key).await;
        self.redis.sadd(&set_key, &hash_key).await;
    }

    /// Cache logs for a specific block (called by block tracker).
    pub async fn cache_logs(&self, chain_id: u64, block_number: u64, logs: &Value) {
        let key = keys::block_logs_key(chain_id, block_number);
        let value = serde_json::to_string(logs).unwrap_or_default();
        self.redis
            .set(&key, &value, std::time::Duration::from_secs(60))
            .await;

        let set_key = keys::head_cache_set_key(chain_id);
        self.redis.sadd(&set_key, &key).await;
    }

    /// Get cached logs for a block.
    pub async fn get_logs(&self, chain_id: u64, block_number: u64) -> Option<Value> {
        let key = keys::block_logs_key(chain_id, block_number);
        let raw = self.redis.get(&key).await?;
        serde_json::from_str(&raw).ok()
    }

    /// Invalidate all head cache entries (on reorg).
    pub async fn invalidate_head_cache(&self, chain_id: u64, chain_name: &str) {
        let set_key = keys::head_cache_set_key(chain_id);
        debug!(chain = %chain_name, "invalidating head cache");
        self.redis.delete_set_and_members(&set_key).await;
    }
}
