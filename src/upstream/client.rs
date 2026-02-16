use anyhow::Result;
use reqwest::Client;
use reqwest::header;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tracing::debug;

use crate::config::{UpstreamConfig, UpstreamRole};
use crate::rpc::types::{JsonRpcRequest, JsonRpcResponse};

const USER_AGENT: &str = concat!("meddler/", env!("CARGO_PKG_VERSION"));

/// An individual upstream RPC node.
pub struct UpstreamClient {
    pub id: String,
    pub role: UpstreamRole,
    pub http_url: String,
    pub ws_url: Option<String>,
    pub max_rps: u32,
    client: Client,
    healthy: AtomicBool,
    block_height: AtomicU64,
}

impl UpstreamClient {
    pub fn new(config: &UpstreamConfig) -> Result<Self> {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());

        let client = Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(32)
            .build()?;

        Ok(Self {
            id: config.id.clone(),
            role: config.role,
            http_url: config.http_url.clone(),
            ws_url: config.ws_url.clone(),
            max_rps: config.max_rps,
            client,
            healthy: AtomicBool::new(true),
            block_height: AtomicU64::new(0),
        })
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub fn set_healthy(&self, healthy: bool) {
        self.healthy.store(healthy, Ordering::Relaxed);
    }

    pub fn block_height(&self) -> u64 {
        self.block_height.load(Ordering::Relaxed)
    }

    pub fn set_block_height(&self, height: u64) {
        self.block_height.store(height, Ordering::Relaxed);
    }

    /// Send a JSON-RPC request to this upstream and return the response.
    pub async fn send_request(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse> {
        let body = serde_json::to_vec(req)?;

        let start = std::time::Instant::now();
        let response = self.client.post(&self.http_url).body(body).send().await?;

        let elapsed = start.elapsed();
        debug!(upstream = %self.id, method = %req.method, elapsed_ms = %elapsed.as_millis(), "upstream response");

        metrics::histogram!("meddler_upstream_request_duration_seconds", "upstream" => self.id.clone())
            .record(elapsed.as_secs_f64());

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "upstream {} returned HTTP {}: {}",
                self.id,
                status,
                body_text
            );
        }

        let resp: JsonRpcResponse = response.json().await?;
        Ok(resp)
    }

    /// Send a raw JSON-RPC call (convenience for internal calls like eth_blockNumber).
    pub async fn call_method(&self, method: &str, params: Value) -> Result<JsonRpcResponse> {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
            id: Value::Number(1.into()),
        };
        self.send_request(&req).await
    }
}
