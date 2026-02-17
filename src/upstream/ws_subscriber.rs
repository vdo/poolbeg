use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
        info!("[{}] upstream WebSocket subscriber started", self.chain_name);

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
                if upstream.role == *role && upstream.is_healthy() && !upstream.is_disabled() {
                    if let Some(ref ws_url) = upstream.ws_url {
                        return Some((upstream.id.clone(), ws_url.clone()));
                    }
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
            .send(Message::Text(
                serde_json::to_string(&subscribe_msg)?.into(),
            ))
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
                            let sub_id = result
                                .as_str()
                                .unwrap_or("")
                                .to_string();
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
                    if parsed.get("method").and_then(|m| m.as_str())
                        != Some("eth_subscription")
                    {
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
                    info!(
                        "[{}] upstream WebSocket closed by server",
                        self.chain_name
                    );
                    return Ok(());
                }
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
            }
        }
    }
}
