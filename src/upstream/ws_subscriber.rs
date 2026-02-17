use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, trace, warn};

use crate::config::UpstreamRole;
use crate::upstream::client::UpstreamClient;
use crate::upstream::tracker::BlockEvent;

/// Maintains a single upstream WebSocket subscription per chain,
/// forwarding raw `newHeads` notifications through the shared broadcast channel.
/// Falls back to HTTP polling (via BlockTracker) when WS is unavailable.
pub struct WsSubscriber {
    chain_id: u64,
    chain_name: String,
    event_tx: broadcast::Sender<BlockEvent>,
    ws_connected: Arc<AtomicBool>,
    upstreams: Arc<Vec<Arc<UpstreamClient>>>,
}

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(30);

impl WsSubscriber {
    pub fn new(
        chain_id: u64,
        chain_name: String,
        event_tx: broadcast::Sender<BlockEvent>,
        ws_connected: Arc<AtomicBool>,
        upstreams: Arc<Vec<Arc<UpstreamClient>>>,
    ) -> Self {
        Self {
            chain_id,
            chain_name,
            event_tx,
            ws_connected,
            upstreams,
        }
    }

    /// Main loop: find upstream with ws_url, connect, subscribe, forward events.
    /// Reconnects with exponential backoff on failure.
    pub async fn run(&self) {
        info!(
            "[{}] upstream WebSocket subscriber started",
            self.chain_name
        );

        let mut backoff = MIN_BACKOFF;

        loop {
            let (upstream_id, ws_url) = match self.find_ws_upstream() {
                Some(pair) => pair,
                None => {
                    debug!(
                        "[{}] no upstream with ws_url available, retrying in {:?}",
                        self.chain_name, backoff
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            };

            info!(
                upstream = %upstream_id,
                "[{}] connecting to upstream WebSocket", self.chain_name
            );

            match self.connect_and_subscribe(&ws_url).await {
                Ok(()) => {
                    // Clean disconnect — reset backoff
                    backoff = MIN_BACKOFF;
                }
                Err(e) => {
                    warn!(
                        upstream = %upstream_id,
                        error = %e,
                        "[{}] upstream WebSocket error", self.chain_name
                    );
                }
            }

            self.ws_connected.store(false, Ordering::Relaxed);
            metrics::gauge!(
                "poolbeg_ws_upstream_connected",
                "chain" => self.chain_name.clone()
            )
            .set(0.0);

            info!(
                "[{}] reconnecting upstream WebSocket in {:?}",
                self.chain_name, backoff
            );
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// Pick the first healthy upstream with a ws_url, by role priority.
    fn find_ws_upstream(&self) -> Option<(String, String)> {
        for role in &[
            UpstreamRole::Primary,
            UpstreamRole::Secondary,
            UpstreamRole::Fallback,
        ] {
            for upstream in self.upstreams.iter() {
                if upstream.role == *role
                    && upstream.is_healthy()
                    && !upstream.is_disabled()
                    && let Some(ref ws_url) = upstream.ws_url
                {
                    return Some((upstream.id.clone(), ws_url.clone()));
                }
            }
        }
        None
    }

    /// Connect to upstream WS, subscribe to newHeads, forward events until disconnect.
    async fn connect_and_subscribe(&self, ws_url: &str) -> anyhow::Result<()> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url).await?;
        let (mut write, mut read) = ws_stream.split();

        // Send eth_subscribe for newHeads
        let subscribe_msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_subscribe",
            "params": ["newHeads"],
            "id": 1
        });
        write
            .send(Message::Text(serde_json::to_string(&subscribe_msg)?.into()))
            .await?;

        // Read subscription confirmation
        let sub_id = self.read_subscription_id(&mut read).await?;

        info!(
            sub_id = %sub_id,
            "[{}] upstream newHeads subscription active", self.chain_name
        );

        self.ws_connected.store(true, Ordering::Relaxed);
        metrics::gauge!(
            "poolbeg_ws_upstream_connected",
            "chain" => self.chain_name.clone()
        )
        .set(1.0);

        // Forward notifications
        self.forward_notifications(&sub_id, &mut read, &mut write)
            .await
    }

    /// Wait for the eth_subscribe response and extract the subscription ID.
    async fn read_subscription_id<S>(&self, read: &mut S) -> anyhow::Result<String>
    where
        S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    {
        loop {
            match tokio::time::timeout(SUBSCRIBE_TIMEOUT, read.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    let parsed: Value = serde_json::from_str(&text)?;
                    if parsed.get("id") == Some(&Value::Number(1.into())) {
                        if let Some(result) = parsed.get("result") {
                            let sub_id = result.as_str().unwrap_or("").to_string();
                            if sub_id.is_empty() {
                                anyhow::bail!("empty subscription ID in response");
                            }
                            return Ok(sub_id);
                        } else if let Some(error) = parsed.get("error") {
                            anyhow::bail!("eth_subscribe failed: {}", error);
                        }
                    }
                }
                Ok(Some(Ok(Message::Close(_)))) => {
                    anyhow::bail!("upstream closed before subscription confirmed");
                }
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(e))) => return Err(e.into()),
                Ok(None) => anyhow::bail!("upstream stream ended before subscription confirmed"),
                Err(_) => anyhow::bail!("timeout waiting for subscription confirmation"),
            }
        }
    }

    /// Main notification loop: read newHeads and emit BlockEvent::NewBlock.
    async fn forward_notifications<S, W>(
        &self,
        sub_id: &str,
        read: &mut S,
        write: &mut W,
    ) -> anyhow::Result<()>
    where
        S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
        W: SinkExt<Message> + Unpin,
    {
        let mut last_block_number: u64 = 0;

        loop {
            match read.next().await {
                Some(Ok(Message::Text(text))) => {
                    let parsed: Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!(
                                error = %e,
                                "[{}] failed to parse upstream WS message", self.chain_name
                            );
                            continue;
                        }
                    };

                    // Only process eth_subscription notifications
                    if parsed.get("method").and_then(|m| m.as_str()) != Some("eth_subscription") {
                        continue;
                    }

                    let params = match parsed.get("params") {
                        Some(p) => p,
                        None => continue,
                    };

                    // Verify it's our subscription
                    let msg_sub_id = params
                        .get("subscription")
                        .and_then(|s| s.as_str())
                        .unwrap_or("");
                    if msg_sub_id != sub_id {
                        continue;
                    }

                    let header = match params.get("result") {
                        Some(h) => h.clone(),
                        None => continue,
                    };

                    let number = header
                        .get("number")
                        .and_then(|n| n.as_str())
                        .and_then(|h| u64::from_str_radix(h.trim_start_matches("0x"), 16).ok())
                        .unwrap_or(0);

                    let hash = header
                        .get("hash")
                        .and_then(|h| h.as_str())
                        .unwrap_or("")
                        .to_string();

                    if number == 0 || hash.is_empty() {
                        warn!(
                            "[{}] upstream WS newHeads missing number or hash",
                            self.chain_name
                        );
                        continue;
                    }

                    if number <= last_block_number {
                        continue;
                    }

                    trace!(
                        number,
                        hash = %hash,
                        "[{}] upstream WS newHeads", self.chain_name
                    );

                    let _ = self.event_tx.send(BlockEvent::NewBlock {
                        chain_id: self.chain_id,
                        number,
                        hash,
                        header,
                    });

                    last_block_number = number;
                }
                Some(Ok(Message::Ping(data))) => {
                    let _ = write.send(Message::Pong(data)).await;
                }
                Some(Ok(Message::Close(_))) => {
                    info!("[{}] upstream WebSocket closed by server", self.chain_name);
                    return Ok(());
                }
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{UpstreamConfig, UpstreamRole};
    use futures_util::{sink::drain, stream};

    fn make_upstream(id: &str, role: UpstreamRole, ws_url: Option<&str>) -> Arc<UpstreamClient> {
        Arc::new(
            UpstreamClient::new(
                &UpstreamConfig {
                    id: id.to_string(),
                    role,
                    http_url: "http://localhost:8545".to_string(),
                    ws_url: ws_url.map(|s| s.to_string()),
                    max_rps: 100,
                },
                Duration::from_secs(86400),
                "test".to_string(),
            )
            .unwrap(),
        )
    }

    fn make_subscriber(
        upstreams: Vec<Arc<UpstreamClient>>,
    ) -> (WsSubscriber, broadcast::Receiver<BlockEvent>) {
        let (event_tx, event_rx) = broadcast::channel(64);
        let ws_connected = Arc::new(AtomicBool::new(false));
        let subscriber = WsSubscriber::new(
            1,
            "test".to_string(),
            event_tx,
            ws_connected,
            Arc::new(upstreams),
        );
        (subscriber, event_rx)
    }

    #[allow(clippy::result_large_err)]
    fn text_msg(
        json: &serde_json::Value,
    ) -> Result<Message, tokio_tungstenite::tungstenite::Error> {
        Ok(Message::Text(serde_json::to_string(json).unwrap().into()))
    }

    // ── find_ws_upstream ──────────────────────────────────────────────

    #[test]
    fn test_find_ws_upstream_none_without_ws_url() {
        let u = make_upstream("a", UpstreamRole::Primary, None);
        let (sub, _rx) = make_subscriber(vec![u]);
        assert!(sub.find_ws_upstream().is_none());
    }

    #[test]
    fn test_find_ws_upstream_returns_upstream_with_ws_url() {
        let u = make_upstream("a", UpstreamRole::Primary, Some("wss://example.com"));
        let (sub, _rx) = make_subscriber(vec![u]);
        let (id, url) = sub.find_ws_upstream().unwrap();
        assert_eq!(id, "a");
        assert_eq!(url, "wss://example.com");
    }

    #[test]
    fn test_find_ws_upstream_picks_primary_over_secondary() {
        let primary = make_upstream("primary", UpstreamRole::Primary, Some("wss://primary"));
        let secondary = make_upstream(
            "secondary",
            UpstreamRole::Secondary,
            Some("wss://secondary"),
        );
        // Insert secondary first to ensure it's not just picking first in vec
        let (sub, _rx) = make_subscriber(vec![secondary, primary]);
        let (id, url) = sub.find_ws_upstream().unwrap();
        assert_eq!(id, "primary");
        assert_eq!(url, "wss://primary");
    }

    #[test]
    fn test_find_ws_upstream_picks_secondary_over_fallback() {
        let secondary = make_upstream(
            "secondary",
            UpstreamRole::Secondary,
            Some("wss://secondary"),
        );
        let fallback = make_upstream("fallback", UpstreamRole::Fallback, Some("wss://fallback"));
        let (sub, _rx) = make_subscriber(vec![fallback, secondary]);
        let (id, _) = sub.find_ws_upstream().unwrap();
        assert_eq!(id, "secondary");
    }

    #[test]
    fn test_find_ws_upstream_skips_unhealthy() {
        let primary = make_upstream("primary", UpstreamRole::Primary, Some("wss://primary"));
        primary.set_healthy(false);
        let secondary = make_upstream(
            "secondary",
            UpstreamRole::Secondary,
            Some("wss://secondary"),
        );
        let (sub, _rx) = make_subscriber(vec![primary, secondary]);
        let (id, _) = sub.find_ws_upstream().unwrap();
        assert_eq!(id, "secondary");
    }

    #[test]
    fn test_find_ws_upstream_skips_upstream_without_ws_url() {
        let no_ws = make_upstream("no-ws", UpstreamRole::Primary, None);
        let with_ws = make_upstream(
            "with-ws",
            UpstreamRole::Secondary,
            Some("wss://example.com"),
        );
        let (sub, _rx) = make_subscriber(vec![no_ws, with_ws]);
        let (id, _) = sub.find_ws_upstream().unwrap();
        assert_eq!(id, "with-ws");
    }

    #[test]
    fn test_find_ws_upstream_none_when_all_unhealthy() {
        let u = make_upstream("a", UpstreamRole::Primary, Some("wss://example.com"));
        u.set_healthy(false);
        let (sub, _rx) = make_subscriber(vec![u]);
        assert!(sub.find_ws_upstream().is_none());
    }

    // ── read_subscription_id ──────────────────────────────────────────

    #[tokio::test]
    async fn test_read_subscription_id_valid() {
        let (sub, _rx) = make_subscriber(vec![]);
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": "0xabc123"
        });
        let mut stream = stream::iter(vec![text_msg(&msg)]);
        let sub_id = sub.read_subscription_id(&mut stream).await.unwrap();
        assert_eq!(sub_id, "0xabc123");
    }

    #[tokio::test]
    async fn test_read_subscription_id_skips_non_matching_id() {
        let (sub, _rx) = make_subscriber(vec![]);
        let wrong = serde_json::json!({"jsonrpc": "2.0", "id": 99, "result": "0xwrong"});
        let correct = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": "0xcorrect"});
        let mut stream = stream::iter(vec![text_msg(&wrong), text_msg(&correct)]);
        let sub_id = sub.read_subscription_id(&mut stream).await.unwrap();
        assert_eq!(sub_id, "0xcorrect");
    }

    #[tokio::test]
    async fn test_read_subscription_id_skips_binary_messages() {
        let (sub, _rx) = make_subscriber(vec![]);
        let valid = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": "0xok"});
        let items: Vec<Result<Message, tokio_tungstenite::tungstenite::Error>> = vec![
            Ok(Message::Binary(vec![0x00, 0x01].into())),
            text_msg(&valid),
        ];
        let mut stream = stream::iter(items);
        let sub_id = sub.read_subscription_id(&mut stream).await.unwrap();
        assert_eq!(sub_id, "0xok");
    }

    #[tokio::test]
    async fn test_read_subscription_id_error_response() {
        let (sub, _rx) = make_subscriber(vec![]);
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32000, "message": "not supported"}
        });
        let mut stream = stream::iter(vec![text_msg(&msg)]);
        let result = sub.read_subscription_id(&mut stream).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("eth_subscribe failed")
        );
    }

    #[tokio::test]
    async fn test_read_subscription_id_empty_result() {
        let (sub, _rx) = make_subscriber(vec![]);
        let msg = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": ""});
        let mut stream = stream::iter(vec![text_msg(&msg)]);
        let result = sub.read_subscription_id(&mut stream).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("empty subscription ID")
        );
    }

    #[tokio::test]
    async fn test_read_subscription_id_close_before_confirm() {
        let (sub, _rx) = make_subscriber(vec![]);
        let items: Vec<Result<Message, tokio_tungstenite::tungstenite::Error>> =
            vec![Ok(Message::Close(None))];
        let mut stream = stream::iter(items);
        let result = sub.read_subscription_id(&mut stream).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("closed before subscription")
        );
    }

    #[tokio::test]
    async fn test_read_subscription_id_stream_ends() {
        let (sub, _rx) = make_subscriber(vec![]);
        let items: Vec<Result<Message, tokio_tungstenite::tungstenite::Error>> = vec![];
        let mut stream = stream::iter(items);
        let result = sub.read_subscription_id(&mut stream).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("stream ended before subscription")
        );
    }

    // ── forward_notifications ─────────────────────────────────────────

    fn new_heads_msg(sub_id: &str, number: &str, hash: &str) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_subscription",
            "params": {
                "subscription": sub_id,
                "result": {
                    "number": number,
                    "hash": hash,
                    "parentHash": "0x0000"
                }
            }
        })
    }

    #[tokio::test]
    async fn test_forward_emits_new_block() {
        let (sub, mut rx) = make_subscriber(vec![]);
        let msg = new_heads_msg("0xsub1", "0xa", "0xdeadbeef");
        let mut read = stream::iter(vec![text_msg(&msg)]);
        let mut write = drain::<Message>();

        let _ = sub
            .forward_notifications("0xsub1", &mut read, &mut write)
            .await;

        let event = rx.try_recv().unwrap();
        match event {
            BlockEvent::NewBlock {
                chain_id,
                number,
                hash,
                header,
            } => {
                assert_eq!(chain_id, 1);
                assert_eq!(number, 10);
                assert_eq!(hash, "0xdeadbeef");
                assert_eq!(header["parentHash"], "0x0000");
            }
            _ => panic!("expected NewBlock event"),
        }
    }

    #[tokio::test]
    async fn test_forward_preserves_raw_header() {
        let (sub, mut rx) = make_subscriber(vec![]);
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_subscription",
            "params": {
                "subscription": "0xs",
                "result": {
                    "number": "0x1",
                    "hash": "0xh",
                    "baseFeePerGas": "0x131c940",
                    "gasLimit": "0x4000000000000",
                    "miner": "0xa4b000000000000000000073657175656e636572"
                }
            }
        });
        let mut read = stream::iter(vec![text_msg(&msg)]);
        let mut write = drain::<Message>();

        let _ = sub
            .forward_notifications("0xs", &mut read, &mut write)
            .await;

        match rx.try_recv().unwrap() {
            BlockEvent::NewBlock { header, .. } => {
                assert_eq!(header["baseFeePerGas"], "0x131c940");
                assert_eq!(header["gasLimit"], "0x4000000000000");
                assert_eq!(
                    header["miner"],
                    "0xa4b000000000000000000073657175656e636572"
                );
                // Should NOT have fields like "transactions", "size", "uncles"
                assert!(header.get("transactions").is_none());
                assert!(header.get("size").is_none());
                assert!(header.get("uncles").is_none());
            }
            _ => panic!("expected NewBlock"),
        }
    }

    #[tokio::test]
    async fn test_forward_deduplicates_blocks() {
        let (sub, mut rx) = make_subscriber(vec![]);
        let mut read = stream::iter(vec![
            text_msg(&new_heads_msg("0xs", "0xa", "0xh1")),
            text_msg(&new_heads_msg("0xs", "0xa", "0xh2")), // same height, deduplicated
            text_msg(&new_heads_msg("0xs", "0x9", "0xh3")), // older, deduplicated
            text_msg(&new_heads_msg("0xs", "0xb", "0xh4")), // new block
        ]);
        let mut write = drain::<Message>();

        let _ = sub
            .forward_notifications("0xs", &mut read, &mut write)
            .await;

        match rx.try_recv().unwrap() {
            BlockEvent::NewBlock { number, .. } => assert_eq!(number, 10),
            _ => panic!("expected NewBlock"),
        }
        match rx.try_recv().unwrap() {
            BlockEvent::NewBlock { number, .. } => assert_eq!(number, 11),
            _ => panic!("expected NewBlock"),
        }
        assert!(rx.try_recv().is_err(), "should only emit 2 events");
    }

    #[tokio::test]
    async fn test_forward_ignores_wrong_sub_id() {
        let (sub, mut rx) = make_subscriber(vec![]);
        let msg = new_heads_msg("0xother", "0xa", "0xh");
        let mut read = stream::iter(vec![text_msg(&msg)]);
        let mut write = drain::<Message>();

        let _ = sub
            .forward_notifications("0xsub1", &mut read, &mut write)
            .await;

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_forward_ignores_non_subscription_messages() {
        let (sub, mut rx) = make_subscriber(vec![]);
        let msg = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": "0xsomething"});
        let mut read = stream::iter(vec![text_msg(&msg)]);
        let mut write = drain::<Message>();

        let _ = sub
            .forward_notifications("0xs", &mut read, &mut write)
            .await;

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_forward_skips_missing_number_or_hash() {
        let (sub, mut rx) = make_subscriber(vec![]);
        let no_number = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_subscription",
            "params": {
                "subscription": "0xs",
                "result": { "hash": "0xh" }
            }
        });
        let no_hash = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_subscription",
            "params": {
                "subscription": "0xs",
                "result": { "number": "0xa" }
            }
        });
        let mut read = stream::iter(vec![text_msg(&no_number), text_msg(&no_hash)]);
        let mut write = drain::<Message>();

        let _ = sub
            .forward_notifications("0xs", &mut read, &mut write)
            .await;

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_forward_handles_close() {
        let (sub, _rx) = make_subscriber(vec![]);
        let items: Vec<Result<Message, tokio_tungstenite::tungstenite::Error>> =
            vec![Ok(Message::Close(None))];
        let mut read = stream::iter(items);
        let mut write = drain::<Message>();

        let result = sub
            .forward_notifications("0xs", &mut read, &mut write)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_forward_handles_empty_stream() {
        let (sub, _rx) = make_subscriber(vec![]);
        let items: Vec<Result<Message, tokio_tungstenite::tungstenite::Error>> = vec![];
        let mut read = stream::iter(items);
        let mut write = drain::<Message>();

        let result = sub
            .forward_notifications("0xs", &mut read, &mut write)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_forward_skips_malformed_json() {
        let (sub, mut rx) = make_subscriber(vec![]);
        let valid = new_heads_msg("0xs", "0x1", "0xh");
        let items: Vec<Result<Message, tokio_tungstenite::tungstenite::Error>> =
            vec![Ok(Message::Text("not json{{{".into())), text_msg(&valid)];
        let mut read = stream::iter(items);
        let mut write = drain::<Message>();

        let _ = sub
            .forward_notifications("0xs", &mut read, &mut write)
            .await;

        // Should have still processed the valid message
        match rx.try_recv().unwrap() {
            BlockEvent::NewBlock { number, .. } => assert_eq!(number, 1),
            _ => panic!("expected NewBlock"),
        }
    }
}
