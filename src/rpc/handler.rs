use std::collections::HashMap;
use std::sync::Arc;

use axum::{Json, body::Bytes, extract::State, http::StatusCode, response::IntoResponse};
use tracing::{debug, warn};

use crate::cache::CacheLayer;
use crate::cache::keys::cache_key;
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
                    params = %req.params,
                    "[{chain_name}] \u{2190} client request"
                );
            }
            let resp = process_single_request(
                chain_mgr,
                &state.cache,
                chain_name,
                &state.config.chains[chain_idx],
                &state.config.server.blocked_methods,
                debug_client,
                req,
                &state,
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

            let responses = process_batch(
                chain_mgr,
                &state.cache,
                chain_name,
                &state.config.chains[chain_idx],
                &state.config.server.blocked_methods,
                debug_client,
                reqs,
                &state,
            )
            .await;
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

#[allow(clippy::too_many_arguments)]
pub async fn process_single_request(
    chain_mgr: &ChainManager,
    cache: &CacheLayer,
    chain_name: &str,
    chain_config: &crate::config::ChainConfig,
    blocked_methods: &[String],
    debug_client: bool,
    req: JsonRpcRequest,
    state: &AppState,
) -> JsonRpcResponse {
    let original_id = req.id.clone();
    let method = req.method.clone();

    if debug_client {
        debug!(method = %method, "[{chain_name}] processing request");
    }

    // Check blocked methods
    if is_method_blocked(&method, blocked_methods) {
        if debug_client {
            debug!(method = %method, "[{chain_name}] method blocked by blocked_methods config");
        }
        metrics::counter!("poolbeg_blocked_requests_total",
            "chain" => chain_name.to_string(),
            "method" => method
        )
        .increment(1);
        return JsonRpcResponse::error(original_id, -32601, "Method not allowed");
    }

    // Memory intercepts: serve static/known responses without any upstream call
    if let Some(resp) = try_memory_intercept(chain_mgr, chain_name, &method, &original_id) {
        if debug_client {
            debug!(method = %method, "[{chain_name}] served from memory (no upstream call)");
        }
        return resp;
    }

    // Resolve block tags ("latest", "safe", "finalized", "earliest") to concrete block numbers
    let req = if let Some(latest_block) = chain_mgr.latest_block_number() {
        resolve_block_tags(req, latest_block, chain_config.finality_depth)
    } else {
        req
    };

    // Check cache first (unless uncacheable)
    if !is_uncacheable(&method) {
        if let Some(cached) = cache.get(chain_config.chain_id, &req).await {
            if debug_client {
                debug!(method = %method, "[{chain_name}] cache hit");
            }
            metrics::counter!("poolbeg_cache_hits_total", "chain" => chain_name.to_string(), "method" => method.clone()).increment(1);
            let mut resp: JsonRpcResponse = match serde_json::from_str(&cached) {
                Ok(r) => r,
                Err(_) => {
                    // Cached data is corrupted, fall through to upstream
                    if debug_client {
                        debug!(method = %method, "[{chain_name}] corrupted cache entry, fetching from upstream");
                    }
                    return coalesced_forward(
                        chain_mgr,
                        cache,
                        chain_name,
                        chain_config,
                        req,
                        original_id,
                        state,
                    )
                    .await;
                }
            };
            resp.id = original_id;
            return resp;
        }
        metrics::counter!("poolbeg_cache_misses_total", "chain" => chain_name.to_string(), "method" => method.clone()).increment(1);

        // Use request coalescing for cacheable methods
        return coalesced_forward(
            chain_mgr,
            cache,
            chain_name,
            chain_config,
            req,
            original_id,
            state,
        )
        .await;
    }

    forward_to_upstream(chain_mgr, cache, chain_name, chain_config, req, original_id).await
}

/// Try to serve a response from memory without any upstream call.
/// Returns `Some(response)` if the method can be served from memory.
fn try_memory_intercept(
    chain_mgr: &ChainManager,
    chain_name: &str,
    method: &str,
    original_id: &serde_json::Value,
) -> Option<JsonRpcResponse> {
    match method {
        // eth_chainId returns the chain ID as a hex string — this never changes
        "eth_chainId" => {
            let hex_chain_id = format!("0x{:x}", chain_mgr.chain_id);
            metrics::counter!("poolbeg_upstream_calls_saved_total",
                "chain" => chain_name.to_string(),
                "reason" => "memory_intercept"
            )
            .increment(1);
            Some(JsonRpcResponse::success(
                original_id.clone(),
                serde_json::Value::String(hex_chain_id),
            ))
        }
        // net_version returns the chain ID as a decimal string — also static
        "net_version" => {
            let decimal_chain_id = chain_mgr.chain_id.to_string();
            metrics::counter!("poolbeg_upstream_calls_saved_total",
                "chain" => chain_name.to_string(),
                "reason" => "memory_intercept"
            )
            .increment(1);
            Some(JsonRpcResponse::success(
                original_id.clone(),
                serde_json::Value::String(decimal_chain_id),
            ))
        }
        // eth_blockNumber returns the latest block number — BlockTracker already tracks this
        "eth_blockNumber" => {
            if let Some(block_num) = chain_mgr.latest_block_number() {
                let hex_block = format!("0x{:x}", block_num);
                metrics::counter!("poolbeg_upstream_calls_saved_total",
                    "chain" => chain_name.to_string(),
                    "reason" => "memory_intercept"
                )
                .increment(1);
                Some(JsonRpcResponse::success(
                    original_id.clone(),
                    serde_json::Value::String(hex_block),
                ))
            } else {
                None // Not yet tracked, fall through to upstream
            }
        }
        _ => None,
    }
}

/// Process a batch of requests with deduplication.
/// Identical cacheable requests within the batch are executed once and results cloned.
#[allow(clippy::too_many_arguments)]
async fn process_batch(
    chain_mgr: &ChainManager,
    cache: &CacheLayer,
    chain_name: &str,
    chain_config: &crate::config::ChainConfig,
    blocked_methods: &[String],
    debug_client: bool,
    reqs: Vec<JsonRpcRequest>,
    state: &AppState,
) -> Vec<JsonRpcResponse> {
    let mut responses: Vec<Option<JsonRpcResponse>> = vec![None; reqs.len()];
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut dedup_map: Vec<Option<usize>> = vec![None; reqs.len()];

    // Preserve original IDs before consuming requests (duplicates need their own id)
    let original_ids: Vec<serde_json::Value> = reqs.iter().map(|r| r.id.clone()).collect();

    // First pass: identify duplicates among cacheable requests
    for (i, req) in reqs.iter().enumerate() {
        if !is_uncacheable(&req.method) && !is_method_blocked(&req.method, blocked_methods) {
            let key = cache_key(chain_config.chain_id, req);
            if let Some(&first_idx) = seen.get(&key) {
                dedup_map[i] = Some(first_idx);
            } else {
                seen.insert(key, i);
            }
        }
    }

    // Second pass: process unique requests
    for (i, req) in reqs.into_iter().enumerate() {
        if dedup_map[i].is_some() {
            continue;
        }

        let method = req.method.clone();
        if debug_client {
            debug!(
                method = %req.method,
                id = %req.id,
                params = %req.params,
                "[{chain_name}] \u{2190} client request (batch)"
            );
        }
        let resp = process_single_request(
            chain_mgr,
            cache,
            chain_name,
            chain_config,
            blocked_methods,
            debug_client,
            req,
            state,
        )
        .await;
        if debug_client {
            log_client_response(chain_name, &method, &resp);
        }
        responses[i] = Some(resp);
    }

    // Third pass: fill in duplicates by cloning from their original, with correct id
    for i in 0..responses.len() {
        if let Some(first_idx) = dedup_map[i]
            && let Some(ref original_resp) = responses[first_idx]
        {
            let mut cloned = original_resp.clone();
            cloned.id = original_ids[i].clone();
            responses[i] = Some(cloned);
            metrics::counter!("poolbeg_upstream_calls_saved_total",
                "chain" => chain_name.to_string(),
                "reason" => "batch_dedup"
            )
            .increment(1);
        }
    }

    responses.into_iter().map(|r| r.unwrap()).collect()
}

/// Forward to upstream with request coalescing for cacheable methods.
/// If another in-flight request with the same cache key exists, wait for its result
/// instead of making a second upstream call.
async fn coalesced_forward(
    chain_mgr: &ChainManager,
    cache: &CacheLayer,
    chain_name: &str,
    chain_config: &crate::config::ChainConfig,
    req: JsonRpcRequest,
    original_id: serde_json::Value,
    state: &AppState,
) -> JsonRpcResponse {
    let key = cache_key(chain_config.chain_id, &req);
    let chain_name_owned = chain_name.to_string();

    state
        .coalescer
        .coalesce_or_execute(key, &chain_name_owned, || async {
            forward_to_upstream(chain_mgr, cache, chain_name, chain_config, req, original_id).await
        })
        .await
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
                let is_unfinalized = block_ref_is_unfinalized(&req, chain_config.finality_depth);
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
