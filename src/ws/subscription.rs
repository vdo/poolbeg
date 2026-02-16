use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{RwLock, broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::cache::CacheLayer;
use crate::upstream::tracker::BlockEvent;

/// A client's subscription registration.
#[derive(Debug, Clone)]
pub struct ClientSubscription {
    pub subscription_id: String,
    pub sub_type: SubscriptionType,
    /// For log subscriptions, the filter criteria.
    pub log_filter: Option<LogFilter>,
    /// Channel to send notifications to this client.
    pub tx: mpsc::UnboundedSender<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SubscriptionType {
    NewHeads,
    Logs,
    NewPendingTransactions,
    Syncing,
}

#[derive(Debug, Clone)]
pub struct LogFilter {
    pub address: Option<Vec<String>>,
    pub topics: Option<Vec<Option<Vec<String>>>>,
}

impl LogFilter {
    pub fn from_value(v: &Value) -> Self {
        let address = v.get("address").and_then(|a| {
            if let Some(s) = a.as_str() {
                Some(vec![s.to_lowercase()])
            } else {
                a.as_array().map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_lowercase()))
                        .collect()
                })
            }
        });

        let topics = v.get("topics").and_then(|t| {
            t.as_array().map(|arr| {
                arr.iter()
                    .map(|topic| {
                        if topic.is_null() {
                            None
                        } else if let Some(s) = topic.as_str() {
                            Some(vec![s.to_lowercase()])
                        } else {
                            topic.as_array().map(|arr| {
                                arr.iter()
                                    .filter_map(|x| x.as_str().map(|s| s.to_lowercase()))
                                    .collect()
                            })
                        }
                    })
                    .collect()
            })
        });

        Self { address, topics }
    }

    /// Check if a log entry matches this filter.
    pub fn matches(&self, log: &Value) -> bool {
        // Check address
        if let Some(ref addrs) = self.address {
            if let Some(log_addr) = log.get("address").and_then(|a| a.as_str()) {
                if !addrs.iter().any(|a| a.eq_ignore_ascii_case(log_addr)) {
                    return false;
                }
            } else {
                return false;
            }
        }

        // Check topics
        if let Some(ref topics) = self.topics {
            let log_topics = log
                .get("topics")
                .and_then(|t| t.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| t.as_str().map(|s| s.to_lowercase()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            for (i, topic_filter) in topics.iter().enumerate() {
                if let Some(filter_values) = topic_filter {
                    match log_topics.get(i) {
                        Some(log_topic) => {
                            if !filter_values
                                .iter()
                                .any(|f| f.eq_ignore_ascii_case(log_topic))
                            {
                                return false;
                            }
                        }
                        None => return false,
                    }
                }
                // None means "any topic at this position" - skip
            }
        }

        true
    }
}

/// Manages all subscriptions for a single chain.
pub struct SubscriptionManager {
    chain_id: u64,
    chain_name: String,
    #[allow(dead_code)]
    cache: CacheLayer,
    /// Map of subscription_id -> ClientSubscription
    subscriptions: Arc<RwLock<HashMap<String, ClientSubscription>>>,
}

impl SubscriptionManager {
    pub fn new(chain_id: u64, chain_name: String, cache: CacheLayer) -> Self {
        Self {
            chain_id,
            chain_name,
            cache,
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a new subscription and return the subscription ID.
    pub async fn subscribe(
        &self,
        sub_type: SubscriptionType,
        log_filter: Option<LogFilter>,
        tx: mpsc::UnboundedSender<Value>,
    ) -> String {
        let subscription_id = format!("0x{}", uuid::Uuid::new_v4().as_simple());

        let sub = ClientSubscription {
            subscription_id: subscription_id.clone(),
            sub_type: sub_type.clone(),
            log_filter,
            tx,
        };

        self.subscriptions
            .write()
            .await
            .insert(subscription_id.clone(), sub);

        let sub_type_str = match sub_type {
            SubscriptionType::NewHeads => "newHeads",
            SubscriptionType::Logs => "logs",
            SubscriptionType::NewPendingTransactions => "newPendingTransactions",
            SubscriptionType::Syncing => "syncing",
        };

        metrics::gauge!("meddler_ws_active_subscriptions",
            "chain" => self.chain_name.clone(),
            "subscription_type" => sub_type_str.to_string()
        )
        .increment(1.0);

        debug!(sub_id = %subscription_id, sub_type = %sub_type_str, "[{}] new subscription", self.chain_name);
        subscription_id
    }

    /// Remove a subscription.
    pub async fn unsubscribe(&self, subscription_id: &str) -> bool {
        let removed = self.subscriptions.write().await.remove(subscription_id);
        if let Some(sub) = removed {
            let sub_type_str = match sub.sub_type {
                SubscriptionType::NewHeads => "newHeads",
                SubscriptionType::Logs => "logs",
                SubscriptionType::NewPendingTransactions => "newPendingTransactions",
                SubscriptionType::Syncing => "syncing",
            };
            metrics::gauge!("meddler_ws_active_subscriptions",
                "chain" => self.chain_name.clone(),
                "subscription_type" => sub_type_str.to_string()
            )
            .decrement(1.0);
            debug!(sub_id = %subscription_id, "[{}] unsubscribed", self.chain_name);
            true
        } else {
            false
        }
    }

    /// Remove all subscriptions for a given sender (on client disconnect).
    /// Returns the number of subscriptions that were removed.
    pub async fn remove_client_subscriptions(&self, _tx: &mpsc::UnboundedSender<Value>) -> usize {
        let mut subs = self.subscriptions.write().await;
        let before = subs.len();
        subs.retain(|_, sub| !sub.tx.is_closed());
        before - subs.len()
    }

    /// Start listening for block events and dispatching to subscribers.
    pub async fn run(&self, mut event_rx: broadcast::Receiver<BlockEvent>) {
        info!("[{}] subscription manager started", self.chain_name);

        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    self.handle_event(event).await;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "[{}] subscription manager lagged behind block events", self.chain_name);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    info!("[{}] block event channel closed, stopping subscription manager", self.chain_name);
                    break;
                }
            }
        }
    }

    async fn handle_event(&self, event: BlockEvent) {
        match event {
            BlockEvent::NewBlock {
                chain_id, header, ..
            } => {
                if chain_id != self.chain_id {
                    return;
                }
                self.dispatch_new_heads(&header).await;
            }
            BlockEvent::NewLogs { chain_id, logs, .. } => {
                if chain_id != self.chain_id {
                    return;
                }
                self.dispatch_logs(&logs).await;
            }
            BlockEvent::Reorg { chain_id, .. } => {
                if chain_id != self.chain_id {
                    // Reorg handling: subscriptions will naturally get new blocks
                    // as the tracker re-fetches the new chain head
                }
            }
        }
    }

    /// Send newHeads notification to all newHeads subscribers.
    async fn dispatch_new_heads(&self, header: &Value) {
        let subs = self.subscriptions.read().await;
        for sub in subs.values() {
            if sub.sub_type != SubscriptionType::NewHeads {
                continue;
            }
            let notification = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "eth_subscription",
                "params": {
                    "subscription": sub.subscription_id,
                    "result": header
                }
            });
            if sub.tx.send(notification).is_err() {
                debug!(sub_id = %sub.subscription_id, "client disconnected, will clean up");
            }
        }
    }

    /// Send logs notifications to matching log subscribers.
    async fn dispatch_logs(&self, logs: &Value) {
        let log_array = match logs.as_array() {
            Some(a) => a,
            None => return,
        };

        let subs = self.subscriptions.read().await;
        for sub in subs.values() {
            if sub.sub_type != SubscriptionType::Logs {
                continue;
            }

            for log in log_array {
                let matches = match &sub.log_filter {
                    Some(filter) => filter.matches(log),
                    None => true, // No filter = all logs
                };

                if matches {
                    let notification = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "eth_subscription",
                        "params": {
                            "subscription": sub.subscription_id,
                            "result": log
                        }
                    });
                    if sub.tx.send(notification).is_err() {
                        break; // Client disconnected
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_log_filter_from_value_single_address() {
        let v = json!({"address": "0xAbC123"});
        let filter = LogFilter::from_value(&v);
        assert_eq!(filter.address, Some(vec!["0xabc123".to_string()]));
        assert!(filter.topics.is_none());
    }

    #[test]
    fn test_log_filter_from_value_multiple_addresses() {
        let v = json!({"address": ["0xABC", "0xDEF"]});
        let filter = LogFilter::from_value(&v);
        assert_eq!(
            filter.address,
            Some(vec!["0xabc".to_string(), "0xdef".to_string()])
        );
    }

    #[test]
    fn test_log_filter_from_value_with_topics() {
        let v = json!({
            "topics": [
                "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
                null,
                "0x000000000000000000000000abc123"
            ]
        });
        let filter = LogFilter::from_value(&v);
        assert!(filter.address.is_none());
        let topics = filter.topics.unwrap();
        assert_eq!(topics.len(), 3);
        assert!(topics[0].is_some()); // Transfer topic
        assert!(topics[1].is_none()); // null = any
        assert!(topics[2].is_some()); // specific topic
    }

    #[test]
    fn test_log_filter_from_value_empty() {
        let v = json!({});
        let filter = LogFilter::from_value(&v);
        assert!(filter.address.is_none());
        assert!(filter.topics.is_none());
    }

    #[test]
    fn test_log_filter_matches_no_filter() {
        let filter = LogFilter {
            address: None,
            topics: None,
        };
        let log = json!({"address": "0xabc", "topics": ["0x123"]});
        assert!(filter.matches(&log));
    }

    #[test]
    fn test_log_filter_matches_address() {
        let filter = LogFilter {
            address: Some(vec!["0xabc".to_string()]),
            topics: None,
        };
        let matching = json!({"address": "0xABC", "topics": []});
        let non_matching = json!({"address": "0xDEF", "topics": []});

        assert!(filter.matches(&matching)); // case insensitive
        assert!(!filter.matches(&non_matching));
    }

    #[test]
    fn test_log_filter_matches_multiple_addresses() {
        let filter = LogFilter {
            address: Some(vec!["0xabc".to_string(), "0xdef".to_string()]),
            topics: None,
        };
        assert!(filter.matches(&json!({"address": "0xabc", "topics": []})));
        assert!(filter.matches(&json!({"address": "0xDEF", "topics": []})));
        assert!(!filter.matches(&json!({"address": "0x999", "topics": []})));
    }

    #[test]
    fn test_log_filter_matches_topic() {
        let transfer_topic = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
        let filter = LogFilter {
            address: None,
            topics: Some(vec![Some(vec![transfer_topic.to_string()])]),
        };
        let matching = json!({"address": "0xabc", "topics": [transfer_topic]});
        let non_matching = json!({"address": "0xabc", "topics": ["0xother"]});

        assert!(filter.matches(&matching));
        assert!(!filter.matches(&non_matching));
    }

    #[test]
    fn test_log_filter_matches_null_topic_position() {
        let filter = LogFilter {
            address: None,
            topics: Some(vec![
                None, // any topic at position 0
                Some(vec!["0xspecific".to_string()]),
            ]),
        };
        let matching = json!({"address": "0xabc", "topics": ["0xanything", "0xspecific"]});
        let non_matching = json!({"address": "0xabc", "topics": ["0xanything", "0xother"]});

        assert!(filter.matches(&matching));
        assert!(!filter.matches(&non_matching));
    }

    #[test]
    fn test_log_filter_matches_missing_topic_returns_false() {
        let filter = LogFilter {
            address: None,
            topics: Some(vec![
                Some(vec!["0xtopic0".to_string()]),
                Some(vec!["0xtopic1".to_string()]),
            ]),
        };
        // Log only has 1 topic but filter requires 2
        let log = json!({"address": "0xabc", "topics": ["0xtopic0"]});
        assert!(!filter.matches(&log));
    }

    #[test]
    fn test_log_filter_address_missing_in_log() {
        let filter = LogFilter {
            address: Some(vec!["0xabc".to_string()]),
            topics: None,
        };
        let log = json!({"topics": ["0x123"]}); // no address field
        assert!(!filter.matches(&log));
    }

    #[test]
    fn test_log_filter_or_topics() {
        // Topic position 0 matches either of two values
        let filter = LogFilter {
            address: None,
            topics: Some(vec![Some(vec![
                "0xtransfer".to_string(),
                "0xapproval".to_string(),
            ])]),
        };
        assert!(filter.matches(&json!({"address": "0xa", "topics": ["0xtransfer"]})));
        assert!(filter.matches(&json!({"address": "0xa", "topics": ["0xapproval"]})));
        assert!(!filter.matches(&json!({"address": "0xa", "topics": ["0xother"]})));
    }

    #[test]
    fn test_subscription_type_equality() {
        assert_eq!(SubscriptionType::NewHeads, SubscriptionType::NewHeads);
        assert_eq!(SubscriptionType::Logs, SubscriptionType::Logs);
        assert_ne!(SubscriptionType::NewHeads, SubscriptionType::Logs);
    }
}
