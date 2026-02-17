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
    let debug_client = state.config.server.debug_client;

    let request = match RpcRequest::parse(&body) {
        Ok(req) => req,
        Err(e) => {
            warn!(error = %e, "[{chain_name}] invalid JSON-RPC request");
            let resp = JsonRpcResponse::error(serde_json::Value::Null, -32700, "Parse error");
            return (StatusCode::OK, Json(RpcResponse::Single(resp)));
        }
    };

    let response = match request {
        RpcRequest::Single(req) => {
            let method = req.method.clone();
            if debug_client {
                debug!(
                    method = %req.method,
                    id = %req.id,
                    params = %truncate_json(&req.params, 512),
                    "[{chain_name}] \u{2190} client request"
                );
            }
            let resp = process_single_request(
                chain_mgr,
                &state.cache,
                chain_name,
                &state.config.chains[chain_idx],
                &state.config.server.blocked_methods,
                req,
            )
            .await;
            if debug_client {
                log_client_response(chain_name, &method, &resp);
            }
            RpcResponse::Single(resp)
        }
        RpcRequest::Batch(reqs) => {
            // Enforce batch size limit
            let max_batch = state.config.server.max_batch_size;
            if reqs.len() > max_batch {
                let resp = JsonRpcResponse::error(
                    serde_json::Value::Null,
                    -32600,
                    format!(
                        "Batch too large: {} requests exceeds limit of {}",
                        reqs.len(),
                        max_batch
                    ),
                );
                return (StatusCode::OK, Json(RpcResponse::Single(resp)));
            }

            if debug_client {
                debug!(
                    "[{chain_name}] \u{2190} client batch of {} requests",
                    reqs.len()
                );
            }

            let mut responses = Vec::with_capacity(reqs.len());
            for req in reqs {
                let method = req.method.clone();
                if debug_client {
                    debug!(
                        method = %req.method,
                        id = %req.id,
                        params = %truncate_json(&req.params, 512),
                        "[{chain_name}] \u{2190} client request (batch)"
                    );
                }
                let resp = process_single_request(
                    chain_mgr,
                    &state.cache,
                    chain_name,
                    &state.config.chains[chain_idx],
                    &state.config.server.blocked_methods,
                    req,
                )
                .await;
                if debug_client {
                    log_client_response(chain_name, &method, &resp);
                }
                responses.push(resp);
            }
            RpcResponse::Batch(responses)
        }
    };

    (StatusCode::OK, Json(response))
}

fn log_client_response(chain_name: &str, method: &str, resp: &JsonRpcResponse) {
    if let Some(ref err) = resp.error {
        debug!(
            method = %method,
            id = %resp.id,
            error_code = err.code,
            error_msg = %err.message,
            "[{chain_name}] \u{2192} client error"
        );
    } else {
        let result_str = resp
            .result
            .as_ref()
            .map(|r| truncate_json(r, 512))
            .unwrap_or_else(|| "null".to_string());
        debug!(
            method = %method,
            id = %resp.id,
            result = %result_str,
            "[{chain_name}] \u{2192} client response"
        );
    }
}

pub async fn process_single_request(
    chain_mgr: &ChainManager,
    cache: &CacheLayer,
    chain_name: &str,
    chain_config: &crate::config::ChainConfig,
    blocked_methods: &[String],
    req: JsonRpcRequest,
) -> JsonRpcResponse {
    let original_id = req.id.clone();
    let method = req.method.clone();

    debug!(method = %method, "[{chain_name}] processing request");

    // Check blocked methods
    if is_method_blocked(&method, blocked_methods) {
        return JsonRpcResponse::error(original_id, -32601, "Method not allowed");
    }

    // Check cache first (unless uncacheable)
    if !is_uncacheable(&method) {
        if let Some(cached) = cache.get(chain_config.chain_id, &req).await {
            debug!(method = %method, "[{chain_name}] cache hit");
            metrics::counter!("poolbeg_cache_hits_total", "chain" => chain_name.to_string(), "method" => method.clone()).increment(1);
            let mut resp: JsonRpcResponse = match serde_json::from_str(&cached) {
                Ok(r) => r,
                Err(_) => {
                    // Cached data is corrupted, fall through to upstream
                    debug!(method = %method, "[{chain_name}] corrupted cache entry, fetching from upstream");
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
        metrics::counter!("poolbeg_cache_misses_total", "chain" => chain_name.to_string(), "method" => method.clone()).increment(1);
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
            warn!(method = %method, error = %e, "[{chain_name}] upstream request failed");
            metrics::counter!("poolbeg_requests_total",
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
