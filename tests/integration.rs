//! Integration tests for meddler.
//!
//! These tests require:
//!   - A running Redis instance (default: localhost:6379)
//!   - Internet access to public RPC endpoints
//!
//! All tests are `#[ignore]`; run them with:
//!   cargo test --test integration -- --ignored
//!
//! Environment variables:
//!   REDIS_URL  - Redis connection URL (default: redis://localhost:6379)

use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use meddler::config::*;
use meddler::server;

// ---------------------------------------------------------------------------
// Free public RPC endpoints (no API key required)
// ---------------------------------------------------------------------------
const ETH_HTTP: &str = "https://ethereum-rpc.publicnode.com";
const ARB_HTTP: &str = "https://arbitrum-one-rpc.publicnode.com";

// ---------------------------------------------------------------------------
// Shared test server (started once, used by all tests)
// ---------------------------------------------------------------------------

fn redis_url() -> String {
    std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string())
}

fn test_config() -> Config {
    Config {
        server: ServerConfig {
            address: "127.0.0.1".to_string(),
            port: 0,
            metrics: MetricsConfig {
                address: "127.0.0.1".to_string(),
                port: 0,
                path: "/metrics".to_string(),
            },
        },
        cache: CacheConfig {
            redis: RedisConfig {
                url: redis_url(),
                pool_size: 4,
                db: 15, // dedicated DB for tests
            },
            policies: vec![
                // Cache chain ID forever so we can test cache hits
                CachePolicy {
                    methods: vec!["eth_chainId".to_string()],
                    finality: Finality::Any,
                    ttl: TtlValue::Duration(Duration::from_secs(300)),
                },
                // Short TTL for everything else
                CachePolicy {
                    methods: vec!["*".to_string()],
                    finality: Finality::Unfinalized,
                    ttl: TtlValue::Duration(Duration::from_secs(5)),
                },
            ],
        },
        chains: vec![
            ChainConfig {
                name: "ethereum".to_string(),
                chain_id: 1,
                expected_block_time: Duration::from_secs(12),
                finality_depth: 64,
                route: "/ethereum".to_string(),
                upstreams: vec![UpstreamConfig {
                    id: "public-eth".to_string(),
                    role: UpstreamRole::Primary,
                    http_url: ETH_HTTP.to_string(),
                    ws_url: None,
                    max_rps: 10,
                }],
            },
            ChainConfig {
                name: "arbitrum".to_string(),
                chain_id: 42161,
                expected_block_time: Duration::from_millis(250),
                finality_depth: 100,
                route: "/arbitrum".to_string(),
                upstreams: vec![UpstreamConfig {
                    id: "public-arb".to_string(),
                    role: UpstreamRole::Primary,
                    http_url: ARB_HTTP.to_string(),
                    ws_url: None,
                    max_rps: 10,
                }],
            },
        ],
    }
}

static SERVER_ADDR: OnceLock<SocketAddr> = OnceLock::new();

/// Returns the address of the shared test server, starting it on first call.
fn server_addr() -> SocketAddr {
    *SERVER_ADDR.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("failed to create runtime");
            rt.block_on(async {
                let config = test_config();
                let app = server::setup(config)
                    .await
                    .expect("server setup failed (is Redis running?)");
                let listener = TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("failed to bind listener");
                let addr = listener.local_addr().unwrap();
                tx.send(addr).unwrap();
                axum::serve(listener, app).await.unwrap();
            });
        });
        let addr = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("test server failed to start within 30s");
        // Brief pause to let background tasks (health checks, block tracker) spin up
        std::thread::sleep(Duration::from_secs(2));
        addr
    })
}

fn http_url(chain: &str) -> String {
    format!("http://{}/{}", server_addr(), chain)
}

fn ws_url(chain: &str) -> String {
    format!("ws://{}/{}", server_addr(), chain)
}

fn rpc_request(method: &str, params: Value, id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": id
    })
}

// ===========================================================================
//  HTTP tests
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_health_endpoint() {
    let addr = server_addr();
    let resp = reqwest::get(format!("http://{}/health", addr))
        .await
        .expect("health request failed");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
#[ignore]
async fn test_http_eth_block_number_ethereum() {
    let client = reqwest::Client::new();
    let resp = client
        .post(http_url("ethereum"))
        .json(&rpc_request("eth_blockNumber", json!([]), 1))
        .send()
        .await
        .expect("request failed");

    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);
    let result = body["result"].as_str().expect("result should be a hex string");
    assert!(result.starts_with("0x"), "block number should be hex: {result}");
    // Sanity: Ethereum is well past block 1M
    let block = u64::from_str_radix(result.trim_start_matches("0x"), 16).unwrap();
    assert!(block > 1_000_000, "block number suspiciously low: {block}");
}

#[tokio::test]
#[ignore]
async fn test_http_eth_block_number_arbitrum() {
    let client = reqwest::Client::new();
    let resp = client
        .post(http_url("arbitrum"))
        .json(&rpc_request("eth_blockNumber", json!([]), 1))
        .send()
        .await
        .expect("request failed");

    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    let result = body["result"].as_str().expect("result should be hex");
    let block = u64::from_str_radix(result.trim_start_matches("0x"), 16).unwrap();
    assert!(block > 1_000_000, "Arbitrum block number too low: {block}");
}

#[tokio::test]
#[ignore]
async fn test_http_eth_chain_id_ethereum() {
    let client = reqwest::Client::new();
    let resp = client
        .post(http_url("ethereum"))
        .json(&rpc_request("eth_chainId", json!([]), 1))
        .send()
        .await
        .unwrap();

    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["result"], "0x1", "Ethereum chain ID should be 1");
}

#[tokio::test]
#[ignore]
async fn test_http_eth_chain_id_arbitrum() {
    let client = reqwest::Client::new();
    let resp = client
        .post(http_url("arbitrum"))
        .json(&rpc_request("eth_chainId", json!([]), 1))
        .send()
        .await
        .unwrap();

    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["result"], "0xa4b1", "Arbitrum chain ID should be 42161 (0xa4b1)");
}

#[tokio::test]
#[ignore]
async fn test_http_batch_request() {
    let client = reqwest::Client::new();
    let batch = json!([
        rpc_request("eth_blockNumber", json!([]), 1),
        rpc_request("eth_chainId", json!([]), 2),
    ]);

    let resp = client
        .post(http_url("ethereum"))
        .json(&batch)
        .send()
        .await
        .unwrap();

    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    let arr = body.as_array().expect("batch response should be an array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], 1);
    assert_eq!(arr[1]["id"], 2);
    assert!(arr[0]["result"].as_str().unwrap().starts_with("0x"));
    assert_eq!(arr[1]["result"], "0x1");
}

#[tokio::test]
#[ignore]
async fn test_http_invalid_json() {
    let client = reqwest::Client::new();
    let resp = client
        .post(http_url("ethereum"))
        .header("content-type", "application/json")
        .body("not json at all")
        .send()
        .await
        .unwrap();

    assert!(resp.status().is_success()); // JSON-RPC returns 200 even on parse errors
    let body: Value = resp.json().await.unwrap();
    assert!(body["error"].is_object());
    assert_eq!(body["error"]["code"], -32700);
}

#[tokio::test]
#[ignore]
async fn test_http_cache_hit() {
    let client = reqwest::Client::new();

    // First call: cache miss, goes to upstream
    let resp1 = client
        .post(http_url("arbitrum"))
        .json(&rpc_request("eth_chainId", json!([]), 100))
        .send()
        .await
        .unwrap();
    let body1: Value = resp1.json().await.unwrap();
    assert_eq!(body1["result"], "0xa4b1");

    // Second call: should hit cache (eth_chainId has 300s TTL in test config)
    let resp2 = client
        .post(http_url("arbitrum"))
        .json(&rpc_request("eth_chainId", json!([]), 200))
        .send()
        .await
        .unwrap();
    let body2: Value = resp2.json().await.unwrap();
    assert_eq!(body2["result"], "0xa4b1");
    // ID should be from the second request, not the cached one
    assert_eq!(body2["id"], 200);
}

#[tokio::test]
#[ignore]
async fn test_http_nonexistent_route() {
    let addr = server_addr();
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/nonexistent", addr))
        .json(&rpc_request("eth_blockNumber", json!([]), 1))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
}

// ===========================================================================
//  WebSocket tests
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_ws_eth_block_number() {
    let (mut ws, _) = connect_async(ws_url("ethereum"))
        .await
        .expect("WS connection failed");

    let req = serde_json::to_string(&rpc_request("eth_blockNumber", json!([]), 1)).unwrap();
    ws.send(Message::Text(req.into())).await.unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(30), ws.next())
        .await
        .expect("WS response timed out")
        .expect("WS stream ended")
        .expect("WS read error");

    let body: Value = match msg {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("expected text message, got: {other:?}"),
    };

    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);
    let result = body["result"].as_str().unwrap();
    assert!(result.starts_with("0x"));

    ws.close(None).await.ok();
}

#[tokio::test]
#[ignore]
async fn test_ws_eth_chain_id() {
    let (mut ws, _) = connect_async(ws_url("arbitrum"))
        .await
        .expect("WS connection failed");

    let req = serde_json::to_string(&rpc_request("eth_chainId", json!([]), 1)).unwrap();
    ws.send(Message::Text(req.into())).await.unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(30), ws.next())
        .await
        .expect("timeout")
        .expect("stream ended")
        .expect("read error");

    let body: Value = match msg {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("expected text, got: {other:?}"),
    };

    assert_eq!(body["result"], "0xa4b1");

    ws.close(None).await.ok();
}

#[tokio::test]
#[ignore]
async fn test_ws_subscribe_new_heads() {
    // Use Arbitrum — ~250ms block time, so we should get a notification quickly
    let (mut ws, _) = connect_async(ws_url("arbitrum"))
        .await
        .expect("WS connection failed");

    // Subscribe to newHeads
    let sub_req = serde_json::to_string(
        &rpc_request("eth_subscribe", json!(["newHeads"]), 1),
    )
    .unwrap();
    ws.send(Message::Text(sub_req.into())).await.unwrap();

    // Read subscription confirmation
    let msg = tokio::time::timeout(Duration::from_secs(30), ws.next())
        .await
        .expect("subscribe response timed out")
        .expect("stream ended")
        .expect("read error");

    let body: Value = match msg {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("expected text, got: {other:?}"),
    };

    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);
    let sub_id = body["result"]
        .as_str()
        .expect("subscription ID should be a string");
    assert!(sub_id.starts_with("0x"), "sub ID should be hex: {sub_id}");

    // Wait for a newHeads notification (Arbitrum produces blocks every ~250ms,
    // but the block tracker polls at expected_block_time intervals)
    let notification = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(Ok(Message::Text(t))) = ws.next().await {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v.get("method") == Some(&json!("eth_subscription")) {
                    return v;
                }
            }
        }
    })
    .await
    .expect("did not receive newHeads notification within 30s");

    assert_eq!(notification["method"], "eth_subscription");
    let params = &notification["params"];
    assert_eq!(params["subscription"], sub_id);
    // The result should contain a block header with a number field
    let header = &params["result"];
    assert!(
        header.get("number").is_some() || header.get("hash").is_some(),
        "notification should contain a block header: {header}"
    );

    // Unsubscribe
    let unsub_req = serde_json::to_string(
        &rpc_request("eth_unsubscribe", json!([sub_id]), 2),
    )
    .unwrap();
    ws.send(Message::Text(unsub_req.into())).await.unwrap();

    let unsub_msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("unsubscribe timed out")
        .expect("stream ended")
        .expect("read error");

    let unsub_body: Value = match unsub_msg {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("expected text, got: {other:?}"),
    };
    assert_eq!(unsub_body["result"], true);

    ws.close(None).await.ok();
}

#[tokio::test]
#[ignore]
async fn test_ws_invalid_json() {
    let (mut ws, _) = connect_async(ws_url("ethereum"))
        .await
        .expect("WS connection failed");

    ws.send(Message::Text("not json".into())).await.unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("timeout")
        .expect("stream ended")
        .expect("read error");

    let body: Value = match msg {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("expected text, got: {other:?}"),
    };

    assert_eq!(body["error"]["code"], -32700);

    ws.close(None).await.ok();
}

#[tokio::test]
#[ignore]
async fn test_ws_batch_request() {
    let (mut ws, _) = connect_async(ws_url("arbitrum"))
        .await
        .expect("WS connection failed");

    let batch = json!([
        rpc_request("eth_blockNumber", json!([]), 1),
        rpc_request("eth_chainId", json!([]), 2),
    ]);
    let req = serde_json::to_string(&batch).unwrap();
    ws.send(Message::Text(req.into())).await.unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(30), ws.next())
        .await
        .expect("timeout")
        .expect("stream ended")
        .expect("read error");

    let body: Value = match msg {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("expected text, got: {other:?}"),
    };

    let arr = body.as_array().expect("batch response should be array");
    assert_eq!(arr.len(), 2);
    assert!(arr[0]["result"].as_str().unwrap().starts_with("0x"));
    assert_eq!(arr[1]["result"], "0xa4b1");

    ws.close(None).await.ok();
}
