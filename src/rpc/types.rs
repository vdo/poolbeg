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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    pub id: Value,
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

/// Determine if a request references a block tag that is "unfinalized" (latest, pending, safe, earliest).
pub fn block_ref_is_unfinalized(req: &JsonRpcRequest) -> bool {
    let params = &req.params;
    if let Some(arr) = params.as_array() {
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
    // If method is something like eth_blockNumber, it returns latest
    if req.method == "eth_blockNumber" || req.method == "eth_getFilterChanges" {
        return true;
    }
    // Default: assume unfinalized for safety (shorter TTL)
    // unless we can prove it references a finalized block number/hash
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
        assert!(block_ref_is_unfinalized(&req));
    }

    #[test]
    fn test_block_ref_pending_is_unfinalized() {
        let req = make_request("eth_getBalance", json!(["0xabc", "pending"]));
        assert!(block_ref_is_unfinalized(&req));
    }

    #[test]
    fn test_block_ref_finalized_is_not_unfinalized() {
        let req = make_request("eth_getBalance", json!(["0xabc", "finalized"]));
        assert!(!block_ref_is_unfinalized(&req));
    }

    #[test]
    fn test_block_ref_earliest_is_not_unfinalized() {
        let req = make_request("eth_getBalance", json!(["0xabc", "earliest"]));
        assert!(!block_ref_is_unfinalized(&req));
    }

    #[test]
    fn test_eth_block_number_is_unfinalized() {
        let req = make_request("eth_blockNumber", json!([]));
        assert!(block_ref_is_unfinalized(&req));
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
}
