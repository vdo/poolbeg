use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

use crate::config::{ChainConfig, UpstreamConfig, UpstreamRole};

const PUBLIC_ENDPOINTS_URL: &str = "https://evm-public-endpoints.erpc.cloud/";

const DEFAULT_PUBLIC_MAX_RPS: u32 = 10;

#[derive(Debug, Deserialize)]
struct EndpointsResponse {
    #[allow(dead_code)]
    metadata: Option<Metadata>,
    #[serde(flatten)]
    chains: HashMap<String, ChainEndpoints>,
}

#[derive(Debug, Deserialize)]
struct Metadata {
    #[allow(dead_code)]
    #[serde(rename = "totalChains")]
    total_chains: Option<u64>,
    #[allow(dead_code)]
    #[serde(rename = "totalEndpoints")]
    total_endpoints: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ChainEndpoints {
    #[allow(dead_code)]
    #[serde(rename = "chainId")]
    chain_id: u64,
    endpoints: Vec<String>,
}

/// Fetch the public endpoints registry and inject matching endpoints as
/// fallback upstreams into any chain that has `auto_public: true`.
///
/// Existing upstream URLs are deduplicated — if a public endpoint URL already
/// appears in the chain's upstreams, it is skipped.
pub async fn resolve_auto_public(chains: &mut [ChainConfig]) -> Result<()> {
    let needs_public: Vec<_> = chains
        .iter()
        .filter(|c| c.auto_public)
        .map(|c| c.chain_id)
        .collect();

    if needs_public.is_empty() {
        return Ok(());
    }

    info!(
        chains = ?needs_public,
        "fetching public RPC endpoints for auto_public chains"
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .context("failed to build HTTP client for public endpoints")?;

    let resp = client
        .get(PUBLIC_ENDPOINTS_URL)
        .send()
        .await
        .context("failed to fetch public endpoints registry")?;

    if !resp.status().is_success() {
        anyhow::bail!("public endpoints registry returned HTTP {}", resp.status());
    }

    let registry: EndpointsResponse = resp
        .json()
        .await
        .context("failed to parse public endpoints JSON")?;

    for chain in chains.iter_mut() {
        if !chain.auto_public {
            continue;
        }

        let key = chain.chain_id.to_string();
        let Some(entry) = registry.chains.get(&key) else {
            warn!(
                chain_id = chain.chain_id,
                "[{}] no public endpoints found for chain_id", chain.name
            );
            continue;
        };

        // Collect existing HTTP URLs for dedup (owned to avoid borrow conflict)
        let existing_urls: std::collections::HashSet<String> =
            chain.upstreams.iter().map(|u| u.http_url.clone()).collect();

        let mut added = 0usize;
        for (i, url) in entry.endpoints.iter().enumerate() {
            if existing_urls.contains(url.as_str()) {
                continue;
            }

            chain.upstreams.push(UpstreamConfig {
                id: format!("auto-public-{}-{}", chain.chain_id, i),
                role: UpstreamRole::Fallback,
                http_url: url.clone(),
                ws_url: None,
                max_rps: DEFAULT_PUBLIC_MAX_RPS,
            });
            added += 1;
        }

        info!(
            chain_id = chain.chain_id,
            added,
            total_public = entry.endpoints.len(),
            "[{}] injected public endpoints as fallback upstreams",
            chain.name
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_chain(chain_id: u64, auto_public: bool) -> ChainConfig {
        ChainConfig {
            name: format!("test-{}", chain_id),
            chain_id,
            expected_block_time: Duration::from_secs(12),
            finality_depth: 64,
            route: format!("/test-{}", chain_id),
            auto_public,
            strategy: crate::config::UpstreamStrategy::RoundRobin,
            disabled_retry_interval: Duration::from_secs(86400),
            upstreams: vec![UpstreamConfig {
                id: "existing".to_string(),
                role: UpstreamRole::Primary,
                http_url: "https://example.com/rpc".to_string(),
                ws_url: None,
                max_rps: 50,
            }],
        }
    }

    #[test]
    fn test_parse_endpoints_response() {
        let json = r#"{
            "metadata": { "totalChains": 2, "totalEndpoints": 4 },
            "1": { "chainId": 1, "endpoints": ["https://a.com", "https://b.com"] },
            "42161": { "chainId": 42161, "endpoints": ["https://c.com", "https://d.com"] }
        }"#;

        let parsed: EndpointsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.chains.get("1").unwrap().endpoints.len(), 2);
        assert_eq!(parsed.chains.get("42161").unwrap().endpoints.len(), 2);
        assert!(!parsed.chains.contains_key("999"));
    }

    #[test]
    fn test_skip_when_no_auto_public() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let mut chains = vec![make_chain(1, false)];
        // Should return Ok immediately without making any HTTP call
        rt.block_on(async {
            let result = resolve_auto_public(&mut chains).await;
            assert!(result.is_ok());
        });
        // No upstreams added
        assert_eq!(chains[0].upstreams.len(), 1);
    }
}
