use std::future::Future;

use dashmap::DashMap;
use tokio::sync::broadcast;
use tracing::debug;

use crate::rpc::types::JsonRpcResponse;

/// Coalesces identical in-flight requests so only one upstream call is made.
///
/// When multiple clients send the same cacheable request concurrently, the first
/// request executes the upstream call while subsequent requests wait for the
/// broadcast result. Once the first request completes, all waiters receive the
/// same response.
pub struct RequestCoalescer {
    /// Maps cache key → broadcast sender for in-flight requests.
    /// The broadcast carries the serialized JSON response string so it can be
    /// cheaply cloned to all waiters.
    in_flight: DashMap<String, broadcast::Sender<String>>,
}

impl Default for RequestCoalescer {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestCoalescer {
    pub fn new() -> Self {
        Self {
            in_flight: DashMap::new(),
        }
    }

    /// Execute the request with coalescing.
    ///
    /// If no in-flight request exists for this key, insert a broadcast channel
    /// and execute the future. Broadcast the result and remove the key.
    ///
    /// If an in-flight request already exists, subscribe to its broadcast and
    /// wait for the result.
    pub async fn coalesce_or_execute<F, Fut>(
        &self,
        key: String,
        chain_name: &str,
        execute: F,
    ) -> JsonRpcResponse
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = JsonRpcResponse>,
    {
        // Check if there's already an in-flight request for this key
        if let Some(entry) = self.in_flight.get(&key) {
            let mut rx = entry.value().subscribe();
            drop(entry); // Release DashMap read lock

            // Wait for the result from the original request
            match rx.recv().await {
                Ok(serialized) => {
                    debug!("[{chain_name}] coalesced request (waited for in-flight)");
                    metrics::counter!("poolbeg_upstream_calls_saved_total",
                        "chain" => chain_name.to_string(),
                        "reason" => "coalesced"
                    )
                    .increment(1);
                    match serde_json::from_str(&serialized) {
                        Ok(resp) => return resp,
                        Err(_) => {
                            // Deserialization failed, fall through to make own request
                        }
                    }
                }
                Err(_) => {
                    // Sender dropped before we got a result, fall through
                }
            }
        }

        // We're the first — insert a broadcast channel
        let (tx, _) = broadcast::channel(1);
        self.in_flight.insert(key.clone(), tx.clone());

        // Execute the actual upstream request
        let resp = execute().await;

        // Broadcast result to any waiters (ignore error if no receivers)
        if let Ok(serialized) = serde_json::to_string(&resp) {
            let _ = tx.send(serialized);
        }

        // Clean up
        self.in_flight.remove(&key);

        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn test_single_request_executes() {
        let coalescer = RequestCoalescer::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = call_count.clone();

        let resp = coalescer
            .coalesce_or_execute("key1".to_string(), "test", || async move {
                cc.fetch_add(1, Ordering::SeqCst);
                JsonRpcResponse::success(json!(1), json!("0x10"))
            })
            .await;

        assert_eq!(call_count.load(Ordering::SeqCst), 1);
        assert_eq!(resp.result.unwrap(), json!("0x10"));
    }

    #[tokio::test]
    async fn test_concurrent_requests_coalesced() {
        let coalescer = Arc::new(RequestCoalescer::new());
        let call_count = Arc::new(AtomicUsize::new(0));
        let (start_tx, start_rx) = tokio::sync::watch::channel(false);

        let mut handles = vec![];

        for i in 0..5 {
            let coalescer = coalescer.clone();
            let call_count = call_count.clone();
            let mut start_rx = start_rx.clone();

            handles.push(tokio::spawn(async move {
                // Wait for all tasks to be ready
                let _ = start_rx.changed().await;

                coalescer
                    .coalesce_or_execute("same_key".to_string(), "test", || {
                        let cc = call_count.clone();
                        async move {
                            // Simulate some upstream delay
                            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                            cc.fetch_add(1, Ordering::SeqCst);
                            JsonRpcResponse::success(json!(i), json!("0x42"))
                        }
                    })
                    .await
            }));
        }

        // Start all tasks at once
        start_tx.send(true).unwrap();

        let mut results = vec![];
        for handle in handles {
            results.push(handle.await.unwrap());
        }

        // At most a few calls should have been made (ideally 1, but timing can cause a few)
        let calls = call_count.load(Ordering::SeqCst);
        assert!(
            calls <= 3,
            "expected coalescing to reduce calls, got {calls}"
        );

        // All results should have the same value
        for resp in &results {
            assert_eq!(resp.result.as_ref().unwrap(), &json!("0x42"));
        }
    }

    #[tokio::test]
    async fn test_different_keys_not_coalesced() {
        let coalescer = Arc::new(RequestCoalescer::new());
        let call_count = Arc::new(AtomicUsize::new(0));

        let cc1 = call_count.clone();
        let c1 = coalescer.clone();
        let h1 = tokio::spawn(async move {
            c1.coalesce_or_execute("key_a".to_string(), "test", || async move {
                cc1.fetch_add(1, Ordering::SeqCst);
                JsonRpcResponse::success(json!(1), json!("a"))
            })
            .await
        });

        let cc2 = call_count.clone();
        let c2 = coalescer.clone();
        let h2 = tokio::spawn(async move {
            c2.coalesce_or_execute("key_b".to_string(), "test", || async move {
                cc2.fetch_add(1, Ordering::SeqCst);
                JsonRpcResponse::success(json!(2), json!("b"))
            })
            .await
        });

        let _ = h1.await.unwrap();
        let _ = h2.await.unwrap();

        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_cleanup_after_completion() {
        let coalescer = RequestCoalescer::new();

        coalescer
            .coalesce_or_execute("key1".to_string(), "test", || async {
                JsonRpcResponse::success(json!(1), json!("ok"))
            })
            .await;

        // The key should be removed after completion
        assert!(coalescer.in_flight.is_empty());
    }
}
