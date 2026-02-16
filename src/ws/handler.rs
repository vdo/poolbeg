use std::sync::Arc;

use axum::{
    extract::{State, WebSocketUpgrade, ws::{Message, WebSocket}},
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::rpc::handler::process_single_request;
use crate::rpc::types::JsonRpcRequest;
use crate::server::AppState;
use crate::ws::subscription::{LogFilter, SubscriptionManager, SubscriptionType};

pub async fn handle_ws(
    State((state, chain_idx)): State<(Arc<AppState>, usize)>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let chain_name = state.config.chains[chain_idx].name.clone();
    info!(chain = %chain_name, "new WebSocket connection");

    ws.on_upgrade(move |socket| handle_ws_connection(socket, state, chain_idx))
}

async fn handle_ws_connection(socket: WebSocket, state: Arc<AppState>, chain_idx: usize) {
    let chain_name = state.config.chains[chain_idx].name.clone();
    let chain_config = &state.config.chains[chain_idx];
    let chain_mgr = &state.chain_managers[chain_idx];

    metrics::gauge!("meddler_ws_active_connections", "chain" => chain_name.clone()).increment(1.0);

    // Create subscription manager for this connection's chain
    let sub_mgr = Arc::new(SubscriptionManager::new(
        chain_config.chain_id,
        chain_name.clone(),
        state.cache.clone(),
    ));

    // Start subscription event dispatcher
    let event_rx = chain_mgr.subscribe_events();
    let sub_mgr_runner = sub_mgr.clone();
    let dispatcher_handle = tokio::spawn(async move {
        sub_mgr_runner.run(event_rx).await;
    });

    // Channel for subscription notifications -> client
    let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<Value>();

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Task: forward subscription notifications to the WebSocket
    let notify_forward = tokio::spawn(async move {
        while let Some(notification) = notify_rx.recv().await {
            let msg = serde_json::to_string(&notification).unwrap_or_default();
            if ws_tx.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    // Main loop: read messages from client
    while let Some(msg) = ws_rx.next().await {
        let msg = match msg {
            Ok(Message::Text(text)) => text.to_string(),
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(_)) => continue,
            Ok(_) => continue,
            Err(e) => {
                debug!(chain = %chain_name, error = %e, "WebSocket read error");
                break;
            }
        };

        let response = handle_ws_message(
            &msg,
            &state,
            chain_idx,
            &sub_mgr,
            &notify_tx,
        )
        .await;

        if let Some(resp) = response {
            let _ = notify_tx.send(resp);
        }
    }

    // Cleanup: remove all subscriptions for this client
    sub_mgr.remove_client_subscriptions(&notify_tx).await;
    dispatcher_handle.abort();
    notify_forward.abort();

    metrics::gauge!("meddler_ws_active_connections", "chain" => chain_name.clone()).decrement(1.0);
    info!(chain = %chain_name, "WebSocket connection closed");
}

async fn handle_ws_message(
    msg: &str,
    state: &Arc<AppState>,
    chain_idx: usize,
    sub_mgr: &Arc<SubscriptionManager>,
    notify_tx: &mpsc::UnboundedSender<Value>,
) -> Option<Value> {
    let parsed: Value = match serde_json::from_str(msg) {
        Ok(v) => v,
        Err(_) => {
            return Some(serde_json::json!({
                "jsonrpc": "2.0",
                "error": {"code": -32700, "message": "Parse error"},
                "id": null
            }));
        }
    };

    // Handle batch requests
    if parsed.is_array() {
        let arr = parsed.as_array().unwrap();
        let mut responses = Vec::new();
        for item in arr {
            if let Some(resp) = handle_single_ws_request(item, state, chain_idx, sub_mgr, notify_tx).await {
                responses.push(resp);
            }
        }
        return Some(Value::Array(responses));
    }

    handle_single_ws_request(&parsed, state, chain_idx, sub_mgr, notify_tx).await
}

async fn handle_single_ws_request(
    parsed: &Value,
    state: &Arc<AppState>,
    chain_idx: usize,
    sub_mgr: &Arc<SubscriptionManager>,
    notify_tx: &mpsc::UnboundedSender<Value>,
) -> Option<Value> {
    let method = parsed.get("method")?.as_str()?;
    let id = parsed.get("id").cloned().unwrap_or(Value::Null);
    let params = parsed.get("params").cloned().unwrap_or(Value::Array(vec![]));

    match method {
        "eth_subscribe" => {
            let sub_type = params
                .as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let (subscription_type, log_filter) = match sub_type {
                "newHeads" => (SubscriptionType::NewHeads, None),
                "logs" => {
                    let filter = params
                        .as_array()
                        .and_then(|a| a.get(1))
                        .map(LogFilter::from_value);
                    (SubscriptionType::Logs, filter)
                }
                "newPendingTransactions" => (SubscriptionType::NewPendingTransactions, None),
                "syncing" => (SubscriptionType::Syncing, None),
                _ => {
                    return Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "error": {"code": -32602, "message": format!("unsupported subscription type: {}", sub_type)},
                        "id": id
                    }));
                }
            };

            let sub_id = sub_mgr
                .subscribe(subscription_type, log_filter, notify_tx.clone())
                .await;

            Some(serde_json::json!({
                "jsonrpc": "2.0",
                "result": sub_id,
                "id": id
            }))
        }

        "eth_unsubscribe" => {
            let sub_id = params
                .as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let success = sub_mgr.unsubscribe(sub_id).await;

            Some(serde_json::json!({
                "jsonrpc": "2.0",
                "result": success,
                "id": id
            }))
        }

        // Regular JSON-RPC call over WebSocket - goes through cache
        _ => {
            let req = JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                method: method.to_string(),
                params,
                id,
            };

            let chain_mgr = &state.chain_managers[chain_idx];
            let chain_config = &state.config.chains[chain_idx];
            let chain_name = &chain_config.name;

            let resp = process_single_request(
                chain_mgr,
                &state.cache,
                chain_name,
                chain_config,
                req,
            )
            .await;

            Some(serde_json::to_value(resp).unwrap_or(Value::Null))
        }
    }
}
