use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single JSON-RPC 2.0 request.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    pub id: Value,
}

/// A single JSON-RPC 2.0 response.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Incoming request can be a single request or a batch.
#[derive(Debug, Clone)]
pub enum RpcRequest {
    Single(JsonRpcRequest),
    Batch(Vec<JsonRpcRequest>),
}

/// Outgoing response can be a single response or a batch.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum RpcResponse {
    Single(JsonRpcResponse),
    Batch(Vec<JsonRpcResponse>),
}

impl JsonRpcResponse {
    pub fn error(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
            id,
        }
    }

    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            result: Some(result),
            error: None,
            id,
        }
    }
}

impl RpcRequest {
    pub fn parse(body: &[u8]) -> Result<Self, serde_json::Error> {
        // Try batch first (array), then single
        if body.first() == Some(&b'[') {
            let batch: Vec<JsonRpcRequest> = serde_json::from_slice(body)?;
            Ok(RpcRequest::Batch(batch))
        } else {
            let single: JsonRpcRequest = serde_json::from_slice(body)?;
            Ok(RpcRequest::Single(single))
        }
    }
}

/// Normalize a request for cache key generation:
/// - Set id to 0
/// - Keep method and params as-is
pub fn normalize_for_cache(req: &JsonRpcRequest) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": req.method,
        "params": req.params,
        "id": 0
    })
}

/// Check if a method is blocked by any of the configured prefixes.
pub fn is_method_blocked(method: &str, blocked: &[String]) -> bool {
    blocked
        .iter()
        .any(|prefix| method.starts_with(prefix.as_str()))
}

/// Methods that should never be cached.
pub fn is_uncacheable(method: &str) -> bool {
    matches!(
        method,
        "eth_sendRawTransaction"
            | "eth_sendTransaction"
            | "eth_estimateGas"
            | "eth_call"
            | "eth_createAccessList"
            | "eth_gasPrice"
            | "eth_maxPriorityFeePerGas"
            | "eth_feeHistory"
            | "net_listening"
            | "net_peerCount"
            | "web3_clientVersion"
            | "eth_subscribe"
            | "eth_unsubscribe"
    )
}

/// Methods that accept a block tag parameter and the index of that parameter.
/// Used by `resolve_block_tags` to replace "latest"/"safe"/"finalized"/"earliest"
/// with concrete block numbers.
fn block_tag_param_index(method: &str) -> Option<usize> {
    match method {
        "eth_getBalance" | "eth_getCode" | "eth_getTransactionCount" | "eth_getStorageAt" => {
            // Last param is block tag (index 1 for getBalance/getCode/getTransactionCount, 2 for getStorageAt)
            match method {
                "eth_getStorageAt" => Some(2),
                _ => Some(1),
            }
        }
        "eth_getBlockByNumber"
        | "eth_getBlockTransactionCountByNumber"
        | "eth_getUncleCountByBlockNumber"
        | "eth_getUncleByBlockNumber" => Some(0),
        _ => None,
    }
}

/// Replace block tag strings ("latest", "safe", "finalized", "earliest") in request params
/// with concrete hex block numbers. "pending" is left as-is (different semantics).
///
/// - "latest" → current latest_block as hex
/// - "safe" → latest_block - finality_depth (or 0) as hex
/// - "finalized" → latest_block - finality_depth (or 0) as hex
/// - "earliest" → "0x0"
pub fn resolve_block_tags(
    mut req: JsonRpcRequest,
    latest_block: u64,
    finality_depth: u64,
) -> JsonRpcRequest {
    let idx = match block_tag_param_index(&req.method) {
        Some(i) => i,
        None => return req,
    };

    if let Some(arr) = req.params.as_array_mut()
        && let Some(param) = arr.get_mut(idx)
        && let Some(tag) = param.as_str()
    {
        let resolved = match tag {
            "latest" => Some(format!("0x{:x}", latest_block)),
            "safe" | "finalized" => {
                let block = latest_block.saturating_sub(finality_depth);
                Some(format!("0x{:x}", block))
            }
            "earliest" => Some("0x0".to_string()),
            _ => None, // "pending" or already a hex number
        };
        if let Some(resolved_val) = resolved {
            *param = Value::String(resolved_val);
        }
    }

    req
}

/// Determine if a request references a block that is "unfinalized" (closer to head than finality_depth).
/// After block tag resolution, params contain concrete hex block numbers.
pub fn block_ref_is_unfinalized(req: &JsonRpcRequest, finality_depth: u64) -> bool {
    // If the method has a known block tag param index, check if it's been resolved
    if let Some(idx) = block_tag_param_index(&req.method)
        && let Some(arr) = req.params.as_array()
        && let Some(param) = arr.get(idx)
        && let Some(s) = param.as_str()
    {
        match s {
            "latest" | "pending" | "safe" => return true,
            "earliest" | "finalized" => return false,
            _ => {
                // It's a hex block number — we can't know if it's finalized without
                // knowing latest_block here, so fall through to the default
            }
        }
    }

    // Scan all params for remaining block tags (e.g. methods not in block_tag_param_index)
    if let Some(arr) = req.params.as_array() {
        for param in arr {
            if let Some(s) = param.as_str() {
                match s {
                    "latest" | "pending" | "safe" => return true,
                    "earliest" | "finalized" => return false,
                    _ => {}
                }
            }
        }
    }

    // eth_blockNumber and eth_getFilterChanges always return head data
    if req.method == "eth_blockNumber" || req.method == "eth_getFilterChanges" {
        return true;
    }

    // If the block tag was resolved, check if the resolved block number is within finality_depth
    // of the latest block. We stored finality_depth in the function signature for this purpose.
    // For resolved tags, we need to check the actual block number against latest.
    // However, without latest_block here, we default to unfinalized for safety.
    // The resolution already happened, so if "latest" was resolved to a number, it's in the
    // unfinalized range. If "finalized" was resolved, it's in the finalized range.
    // Since we can't distinguish after resolution, we keep the conservative default.
    let _ = finality_depth;

    // Default: assume unfinalized for safety (shorter TTL)
    true
}

/// Truncate a JSON value to a display string of at most `max_len` characters.
/// Appends "..." when truncated and includes the original byte size.
pub fn truncate_json(val: &serde_json::Value, max_len: usize) -> String {
    let s = val.to_string();
    if s.len() <= max_len {
        s
    } else {
        format!("{}... ({} bytes)", &s[..max_len], s.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_request(method: &str, params: Value) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
            id: json!(1),
        }
    }

    #[test]
    fn test_parse_single_request() {
        let body = br#"{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}"#;
        let req = RpcRequest::parse(body).unwrap();
        match req {
            RpcRequest::Single(r) => {
                assert_eq!(r.method, "eth_blockNumber");
                assert_eq!(r.id, json!(1));
            }
            RpcRequest::Batch(_) => panic!("expected single request"),
        }
    }

    #[test]
    fn test_parse_batch_request() {
        let body = br#"[{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1},{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":2}]"#;
        let req = RpcRequest::parse(body).unwrap();
        match req {
            RpcRequest::Batch(batch) => {
                assert_eq!(batch.len(), 2);
                assert_eq!(batch[0].method, "eth_blockNumber");
                assert_eq!(batch[1].method, "eth_chainId");
            }
            RpcRequest::Single(_) => panic!("expected batch request"),
        }
    }

    #[test]
    fn test_parse_invalid_json() {
        let body = b"not json";
        assert!(RpcRequest::parse(body).is_err());
    }

    #[test]
    fn test_normalize_for_cache_strips_id() {
        let req1 = make_request("eth_getBalance", json!(["0xabc", "latest"]));
        let req2 = make_request("eth_getBalance", json!(["0xabc", "latest"]));

        let n1 = normalize_for_cache(&req1);
        let n2 = normalize_for_cache(&req2);

        // Both should produce the same normalized value
        assert_eq!(n1, n2);
        assert_eq!(n1["id"], json!(0));
        assert_eq!(n1["method"], "eth_getBalance");
    }

    #[test]
    fn test_normalize_different_ids_same_result() {
        let req1 = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: "eth_getBalance".to_string(),
            params: json!(["0xabc", "latest"]),
            id: json!(1),
        };
        let req2 = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: "eth_getBalance".to_string(),
            params: json!(["0xabc", "latest"]),
            id: json!(999),
        };

        assert_eq!(normalize_for_cache(&req1), normalize_for_cache(&req2));
    }

    #[test]
    fn test_normalize_different_params_different_result() {
        let req1 = make_request("eth_getBalance", json!(["0xabc", "latest"]));
        let req2 = make_request("eth_getBalance", json!(["0xdef", "latest"]));

        assert_ne!(normalize_for_cache(&req1), normalize_for_cache(&req2));
    }

    #[test]
    fn test_is_uncacheable() {
        assert!(is_uncacheable("eth_sendRawTransaction"));
        assert!(is_uncacheable("eth_estimateGas"));
        assert!(is_uncacheable("eth_call"));
        assert!(is_uncacheable("eth_subscribe"));
        assert!(is_uncacheable("eth_unsubscribe"));
        assert!(is_uncacheable("eth_gasPrice"));

        assert!(!is_uncacheable("eth_getBlockByNumber"));
        assert!(!is_uncacheable("eth_getBalance"));
        assert!(!is_uncacheable("eth_getTransactionByHash"));
        assert!(!is_uncacheable("eth_blockNumber"));
    }

    #[test]
    fn test_block_ref_latest_is_unfinalized() {
        let req = make_request("eth_getBalance", json!(["0xabc", "latest"]));
        assert!(block_ref_is_unfinalized(&req, 64));
    }

    #[test]
    fn test_block_ref_pending_is_unfinalized() {
        let req = make_request("eth_getBalance", json!(["0xabc", "pending"]));
        assert!(block_ref_is_unfinalized(&req, 64));
    }

    #[test]
    fn test_block_ref_finalized_is_not_unfinalized() {
        let req = make_request("eth_getBalance", json!(["0xabc", "finalized"]));
        assert!(!block_ref_is_unfinalized(&req, 64));
    }

    #[test]
    fn test_block_ref_earliest_is_not_unfinalized() {
        let req = make_request("eth_getBalance", json!(["0xabc", "earliest"]));
        assert!(!block_ref_is_unfinalized(&req, 64));
    }

    #[test]
    fn test_eth_block_number_is_unfinalized() {
        let req = make_request("eth_blockNumber", json!([]));
        assert!(block_ref_is_unfinalized(&req, 64));
    }

    #[test]
    fn test_response_error() {
        let resp = JsonRpcResponse::error(json!(1), -32600, "Invalid Request");
        assert!(resp.error.is_some());
        assert!(resp.result.is_none());
        assert_eq!(resp.error.unwrap().code, -32600);
    }

    #[test]
    fn test_response_success() {
        let resp = JsonRpcResponse::success(json!(1), json!("0x10"));
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
        assert_eq!(resp.result.unwrap(), json!("0x10"));
    }

    #[test]
    fn test_is_method_blocked() {
        let blocked = vec![
            "admin_".to_string(),
            "debug_".to_string(),
            "personal_".to_string(),
        ];
        assert!(is_method_blocked("admin_nodeInfo", &blocked));
        assert!(is_method_blocked("debug_traceTransaction", &blocked));
        assert!(is_method_blocked("personal_unlockAccount", &blocked));
        assert!(!is_method_blocked("eth_blockNumber", &blocked));
        assert!(!is_method_blocked("eth_call", &blocked));
    }

    #[test]
    fn test_is_method_blocked_empty() {
        let blocked: Vec<String> = vec![];
        assert!(!is_method_blocked("admin_nodeInfo", &blocked));
    }

    // ── resolve_block_tags tests ────────────────────────────────────────

    #[test]
    fn test_resolve_latest_to_block_number() {
        let req = make_request("eth_getBalance", json!(["0xabc", "latest"]));
        let resolved = resolve_block_tags(req, 1000, 64);
        assert_eq!(resolved.params[1], "0x3e8");
    }

    #[test]
    fn test_resolve_finalized_to_block_number() {
        let req = make_request("eth_getBalance", json!(["0xabc", "finalized"]));
        let resolved = resolve_block_tags(req, 1000, 64);
        // 1000 - 64 = 936 = 0x3a8
        assert_eq!(resolved.params[1], "0x3a8");
    }

    #[test]
    fn test_resolve_safe_to_block_number() {
        let req = make_request("eth_getBalance", json!(["0xabc", "safe"]));
        let resolved = resolve_block_tags(req, 1000, 64);
        assert_eq!(resolved.params[1], "0x3a8");
    }

    #[test]
    fn test_resolve_earliest_to_zero() {
        let req = make_request("eth_getBalance", json!(["0xabc", "earliest"]));
        let resolved = resolve_block_tags(req, 1000, 64);
        assert_eq!(resolved.params[1], "0x0");
    }

    #[test]
    fn test_resolve_pending_unchanged() {
        let req = make_request("eth_getBalance", json!(["0xabc", "pending"]));
        let resolved = resolve_block_tags(req, 1000, 64);
        assert_eq!(resolved.params[1], "pending");
    }

    #[test]
    fn test_resolve_hex_number_unchanged() {
        let req = make_request("eth_getBalance", json!(["0xabc", "0x100"]));
        let resolved = resolve_block_tags(req, 1000, 64);
        assert_eq!(resolved.params[1], "0x100");
    }

    #[test]
    fn test_resolve_get_block_by_number_latest() {
        let req = make_request("eth_getBlockByNumber", json!(["latest", false]));
        let resolved = resolve_block_tags(req, 500, 32);
        assert_eq!(resolved.params[0], "0x1f4");
    }

    #[test]
    fn test_resolve_get_storage_at() {
        let req = make_request("eth_getStorageAt", json!(["0xabc", "0x0", "latest"]));
        let resolved = resolve_block_tags(req, 2000, 64);
        assert_eq!(resolved.params[2], "0x7d0");
    }

    #[test]
    fn test_resolve_unknown_method_unchanged() {
        let req = make_request("eth_getTransactionByHash", json!(["0xabc"]));
        let resolved = resolve_block_tags(req.clone(), 1000, 64);
        assert_eq!(resolved.params, req.params);
    }

    #[test]
    fn test_resolve_same_latest_produces_same_cache_key() {
        let req1 = make_request("eth_getBalance", json!(["0xabc", "latest"]));
        let req2 = make_request("eth_getBalance", json!(["0xabc", "latest"]));

        let resolved1 = resolve_block_tags(req1, 1000, 64);
        let resolved2 = resolve_block_tags(req2, 1000, 64);

        assert_eq!(
            normalize_for_cache(&resolved1),
            normalize_for_cache(&resolved2)
        );
    }

    #[test]
    fn test_resolve_finality_depth_saturating() {
        // When finality_depth > latest_block, should saturate at 0
        let req = make_request("eth_getBalance", json!(["0xabc", "finalized"]));
        let resolved = resolve_block_tags(req, 10, 100);
        assert_eq!(resolved.params[1], "0x0");
    }
}
