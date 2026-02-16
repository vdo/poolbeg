use std::sync::Arc;

use axum::{Json, body::Bytes, extract::State, http::StatusCode, response::IntoResponse};
use tracing::{debug, warn};

use crate::cache::CacheLayer;
use crate::rpc::types::*;
use crate::server::AppState;
use crate::upstream::manager::ChainManager;

pub async fn handle_rpc(
    State((state, chain_idx)): State<(Arc<AppState>, usize)>,
    body: Bytes,
) -> impl IntoResponse {
    let chain_mgr = &state.chain_managers[chain_idx];
    let chain_name = &state.config.chains[chain_idx].name;

    let request = match RpcRequest::parse(&body) {
        Ok(req) => req,
        Err(e) => {
            warn!(chain = %chain_name, error = %e, "invalid JSON-RPC request");
            let resp = JsonRpcResponse::error(serde_json::Value::Null, -32700, "Parse error");
            return (StatusCode::OK, Json(RpcResponse::Single(resp)));
        }
    };

    let response = match request {
        RpcRequest::Single(req) => {
            let resp = process_single_request(
                chain_mgr,
                &state.cache,
                chain_name,
                &state.config.chains[chain_idx],
                req,
            )
            .await;
            RpcResponse::Single(resp)
        }
        RpcRequest::Batch(reqs) => {
            let mut responses = Vec::with_capacity(reqs.len());
            for req in reqs {
                let resp = process_single_request(
                    chain_mgr,
                    &state.cache,
                    chain_name,
                    &state.config.chains[chain_idx],
                    req,
                )
                .await;
                responses.push(resp);
            }
            RpcResponse::Batch(responses)
        }
    };

    (StatusCode::OK, Json(response))
}

pub async fn process_single_request(
    chain_mgr: &ChainManager,
    cache: &CacheLayer,
    chain_name: &str,
    chain_config: &crate::config::ChainConfig,
    req: JsonRpcRequest,
) -> JsonRpcResponse {
    let original_id = req.id.clone();
    let method = req.method.clone();

    debug!(chain = %chain_name, method = %method, "processing request");

    // Check cache first (unless uncacheable)
    if !is_uncacheable(&method) {
        if let Some(cached) = cache.get(chain_config.chain_id, &req).await {
            debug!(chain = %chain_name, method = %method, "cache hit");
            metrics::counter!("meddler_cache_hits_total", "chain" => chain_name.to_string(), "method" => method.clone()).increment(1);
            let mut resp: JsonRpcResponse = match serde_json::from_str(&cached) {
                Ok(r) => r,
                Err(_) => {
                    // Cached data is corrupted, fall through to upstream
                    debug!(chain = %chain_name, method = %method, "corrupted cache entry, fetching from upstream");
                    return forward_to_upstream(
                        chain_mgr,
                        cache,
                        chain_name,
                        chain_config,
                        req,
                        original_id,
                    )
                    .await;
                }
            };
            resp.id = original_id;
            return resp;
        }
        metrics::counter!("meddler_cache_misses_total", "chain" => chain_name.to_string(), "method" => method.clone()).increment(1);
    }

    forward_to_upstream(chain_mgr, cache, chain_name, chain_config, req, original_id).await
}

async fn forward_to_upstream(
    chain_mgr: &ChainManager,
    cache: &CacheLayer,
    chain_name: &str,
    chain_config: &crate::config::ChainConfig,
    req: JsonRpcRequest,
    original_id: serde_json::Value,
) -> JsonRpcResponse {
    let method = req.method.clone();

    match chain_mgr.forward_request(&req).await {
        Ok(mut resp) => {
            // Cache the response if cacheable
            if !is_uncacheable(&method) && resp.error.is_none() {
                let is_unfinalized = block_ref_is_unfinalized(&req);
                cache
                    .set(
                        chain_config.chain_id,
                        &req,
                        &resp,
                        is_unfinalized,
                        &chain_config.name,
                    )
                    .await;
            }

            resp.id = original_id;
            resp
        }
        Err(e) => {
            warn!(chain = %chain_name, method = %method, error = %e, "upstream request failed");
            metrics::counter!("meddler_requests_total",
                "chain" => chain_name.to_string(),
                "method" => method,
                "status" => "error",
                "cache_hit" => "false"
            )
            .increment(1);
            JsonRpcResponse::error(original_id, -32603, format!("upstream error: {e}"))
        }
    }
}
