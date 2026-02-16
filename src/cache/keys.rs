use crate::rpc::types::{normalize_for_cache, JsonRpcRequest};
use blake3::Hasher;

/// Generate a Redis cache key for a JSON-RPC request.
/// Format: `meddler:{chain_id}:{blake3_hash}`
pub fn cache_key(chain_id: u64, req: &JsonRpcRequest) -> String {
    let normalized = normalize_for_cache(req);
    let json_bytes = serde_json::to_vec(&normalized).unwrap_or_default();

    let mut hasher = Hasher::new();
    hasher.update(&json_bytes);
    let hash = hasher.finalize();

    format!("meddler:{}:{}", chain_id, hash.to_hex())
}

/// Cache key for a block by number.
pub fn block_by_number_key(chain_id: u64, number: u64) -> String {
    format!("meddler:{}:block:number:{}", chain_id, number)
}

/// Cache key for a block by hash.
pub fn block_by_hash_key(chain_id: u64, hash: &str) -> String {
    format!("meddler:{}:block:hash:{}", chain_id, hash)
}

/// Cache key for logs of a specific block.
pub fn block_logs_key(chain_id: u64, block_number: u64) -> String {
    format!("meddler:{}:logs:{}", chain_id, block_number)
}

/// Redis set key tracking unfinalized (head) cache entries for a chain.
pub fn head_cache_set_key(chain_id: u64) -> String {
    format!("meddler:{}:head_cache", chain_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::types::JsonRpcRequest;
    use serde_json::json;

    fn make_request(method: &str, params: serde_json::Value) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
            id: json!(1),
        }
    }

    #[test]
    fn test_cache_key_format() {
        let req = make_request("eth_blockNumber", json!([]));
        let key = cache_key(1, &req);
        assert!(key.starts_with("meddler:1:"));
        // blake3 hex is 64 chars
        let hash_part = key.strip_prefix("meddler:1:").unwrap();
        assert_eq!(hash_part.len(), 64);
    }

    #[test]
    fn test_cache_key_deterministic() {
        let req1 = make_request("eth_getBalance", json!(["0xabc", "latest"]));
        let req2 = make_request("eth_getBalance", json!(["0xabc", "latest"]));
        assert_eq!(cache_key(1, &req1), cache_key(1, &req2));
    }

    #[test]
    fn test_cache_key_different_ids_same_key() {
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
            id: json!(42),
        };
        // IDs differ but normalized key should be the same
        assert_eq!(cache_key(1, &req1), cache_key(1, &req2));
    }

    #[test]
    fn test_cache_key_different_chain_ids() {
        let req = make_request("eth_blockNumber", json!([]));
        let key1 = cache_key(1, &req);
        let key2 = cache_key(42161, &req);
        assert_ne!(key1, key2);
        assert!(key1.starts_with("meddler:1:"));
        assert!(key2.starts_with("meddler:42161:"));
    }

    #[test]
    fn test_cache_key_different_methods() {
        let req1 = make_request("eth_blockNumber", json!([]));
        let req2 = make_request("eth_chainId", json!([]));
        assert_ne!(cache_key(1, &req1), cache_key(1, &req2));
    }

    #[test]
    fn test_cache_key_different_params() {
        let req1 = make_request("eth_getBalance", json!(["0xaaa", "latest"]));
        let req2 = make_request("eth_getBalance", json!(["0xbbb", "latest"]));
        assert_ne!(cache_key(1, &req1), cache_key(1, &req2));
    }

    #[test]
    fn test_block_by_number_key_format() {
        assert_eq!(
            block_by_number_key(1, 12345),
            "meddler:1:block:number:12345"
        );
        assert_eq!(
            block_by_number_key(42161, 0),
            "meddler:42161:block:number:0"
        );
    }

    #[test]
    fn test_block_by_hash_key_format() {
        let hash = "0xabc123";
        assert_eq!(block_by_hash_key(1, hash), "meddler:1:block:hash:0xabc123");
    }

    #[test]
    fn test_block_logs_key_format() {
        assert_eq!(block_logs_key(1, 100), "meddler:1:logs:100");
    }

    #[test]
    fn test_head_cache_set_key_format() {
        assert_eq!(head_cache_set_key(1), "meddler:1:head_cache");
        assert_eq!(head_cache_set_key(42161), "meddler:42161:head_cache");
    }
}
