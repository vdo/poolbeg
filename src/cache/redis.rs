use anyhow::Result;
use fred::prelude::*;
use std::time::Duration;
use tracing::{debug, warn};

/// Wrapper around the Fred Redis client pool.
#[derive(Clone)]
pub struct RedisClient {
    pool: Pool,
}

impl RedisClient {
    pub async fn new(url: &str, pool_size: usize) -> Result<Self> {
        let config = Config::from_url(url)?;
        let pool = Builder::from_config(config)
            .set_policy(ReconnectPolicy::new_exponential(0, 100, 30_000, 2))
            .build_pool(pool_size)?;

        pool.init().await?;

        Ok(Self { pool })
    }

    /// Get a value from Redis.
    pub async fn get(&self, key: &str) -> Option<String> {
        match self.pool.get::<Option<String>, _>(key).await {
            Ok(v) => v,
            Err(e) => {
                warn!(key = %key, error = %e, "redis GET failed");
                None
            }
        }
    }

    /// Set a value in Redis with optional TTL.
    /// If ttl is Duration::ZERO, set without expiry.
    pub async fn set(&self, key: &str, value: &str, ttl: Duration) {
        let result = if ttl.is_zero() {
            self.pool
                .set::<(), _, _>(key, value, None, None, false)
                .await
        } else {
            let expiry = Expiration::PX(ttl.as_millis() as i64);
            self.pool
                .set::<(), _, _>(key, value, Some(expiry), None, false)
                .await
        };

        if let Err(e) = result {
            warn!(key = %key, error = %e, "redis SET failed");
        }
    }

    /// Delete a key.
    pub async fn del(&self, key: &str) {
        if let Err(e) = self.pool.del::<(), _>(key).await {
            warn!(key = %key, error = %e, "redis DEL failed");
        }
    }

    /// Add a member to a set.
    pub async fn sadd(&self, key: &str, member: &str) {
        if let Err(e) = self.pool.sadd::<(), _, _>(key, member).await {
            warn!(key = %key, error = %e, "redis SADD failed");
        }
    }

    /// Get all members of a set.
    pub async fn smembers(&self, key: &str) -> Vec<String> {
        match self.pool.smembers::<Vec<String>, _>(key).await {
            Ok(v) => v,
            Err(e) => {
                warn!(key = %key, error = %e, "redis SMEMBERS failed");
                Vec::new()
            }
        }
    }

    /// Delete a set and all its tracked keys.
    pub async fn delete_set_and_members(&self, set_key: &str) {
        let members = self.smembers(set_key).await;
        if members.is_empty() {
            return;
        }

        debug!(set_key = %set_key, count = members.len(), "invalidating head cache entries");

        for member in &members {
            self.del(member).await;
        }
        self.del(set_key).await;
    }
}
