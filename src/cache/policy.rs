use std::time::Duration;

use crate::config::{CacheConfig, Finality, TtlValue};

/// Determine the TTL for a given method and finality state based on configured policies.
/// Returns None if the request should not be cached.
pub fn resolve_ttl(config: &CacheConfig, method: &str, is_unfinalized: bool) -> Option<Duration> {
    let finality = if is_unfinalized {
        Finality::Unfinalized
    } else {
        Finality::Finalized
    };

    // Find first matching policy
    for policy in &config.policies {
        if matches_method(&policy.methods, method) && matches_finality(policy.finality, finality) {
            return match policy.ttl {
                TtlValue::Never => None,
                TtlValue::Forever => Some(Duration::from_secs(0)), // 0 = no expiry in Redis
                TtlValue::Duration(d) => Some(d),
            };
        }
    }

    // Default: short TTL for unfinalized, longer for finalized
    if is_unfinalized {
        Some(Duration::from_secs(3))
    } else {
        Some(Duration::from_secs(3600))
    }
}

fn matches_method(patterns: &[String], method: &str) -> bool {
    patterns.iter().any(|p| p == "*" || p == method)
}

fn matches_finality(policy_finality: Finality, request_finality: Finality) -> bool {
    policy_finality == Finality::Any || policy_finality == request_finality
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CacheConfig, CachePolicy, RedisConfig, TtlValue};

    fn make_config(policies: Vec<CachePolicy>) -> CacheConfig {
        CacheConfig {
            redis: RedisConfig {
                url: "redis://localhost:6379".to_string(),
                pool_size: 16,
                db: 0,
            },
            policies,
        }
    }

    fn make_policy(methods: &[&str], finality: Finality, ttl: TtlValue) -> CachePolicy {
        CachePolicy {
            methods: methods.iter().map(|s| s.to_string()).collect(),
            finality,
            ttl,
        }
    }

    #[test]
    fn test_resolve_ttl_exact_method_match() {
        let config = make_config(vec![make_policy(
            &["eth_getBlockByHash"],
            Finality::Finalized,
            TtlValue::Forever,
        )]);
        let ttl = resolve_ttl(&config, "eth_getBlockByHash", false);
        assert_eq!(ttl, Some(Duration::from_secs(0))); // Forever = 0 (no expiry)
    }

    #[test]
    fn test_resolve_ttl_never_cache() {
        let config = make_config(vec![make_policy(
            &["eth_sendRawTransaction"],
            Finality::Any,
            TtlValue::Never,
        )]);
        let ttl = resolve_ttl(&config, "eth_sendRawTransaction", true);
        assert_eq!(ttl, None);
    }

    #[test]
    fn test_resolve_ttl_duration() {
        let config = make_config(vec![make_policy(
            &["eth_getBalance"],
            Finality::Unfinalized,
            TtlValue::Duration(Duration::from_secs(2)),
        )]);
        let ttl = resolve_ttl(&config, "eth_getBalance", true);
        assert_eq!(ttl, Some(Duration::from_secs(2)));
    }

    #[test]
    fn test_resolve_ttl_wildcard_match() {
        let config = make_config(vec![make_policy(
            &["*"],
            Finality::Finalized,
            TtlValue::Duration(Duration::from_secs(3600)),
        )]);
        let ttl = resolve_ttl(&config, "eth_getWhatever", false);
        assert_eq!(ttl, Some(Duration::from_secs(3600)));
    }

    #[test]
    fn test_resolve_ttl_finality_mismatch_skips_policy() {
        let config = make_config(vec![make_policy(
            &["eth_getBalance"],
            Finality::Finalized,
            TtlValue::Forever,
        )]);
        // Request is unfinalized, policy is finalized-only: should not match
        let ttl = resolve_ttl(&config, "eth_getBalance", true);
        // Falls through to default: 3s for unfinalized
        assert_eq!(ttl, Some(Duration::from_secs(3)));
    }

    #[test]
    fn test_resolve_ttl_any_finality_matches_both() {
        let config = make_config(vec![make_policy(
            &["eth_call"],
            Finality::Any,
            TtlValue::Never,
        )]);
        assert_eq!(resolve_ttl(&config, "eth_call", true), None);
        assert_eq!(resolve_ttl(&config, "eth_call", false), None);
    }

    #[test]
    fn test_resolve_ttl_first_match_wins() {
        let config = make_config(vec![
            make_policy(
                &["eth_getBalance"],
                Finality::Unfinalized,
                TtlValue::Duration(Duration::from_secs(2)),
            ),
            make_policy(
                &["*"],
                Finality::Unfinalized,
                TtlValue::Duration(Duration::from_secs(10)),
            ),
        ]);
        let ttl = resolve_ttl(&config, "eth_getBalance", true);
        assert_eq!(ttl, Some(Duration::from_secs(2))); // First match, not wildcard
    }

    #[test]
    fn test_resolve_ttl_default_unfinalized() {
        let config = make_config(vec![]);
        let ttl = resolve_ttl(&config, "unknown_method", true);
        assert_eq!(ttl, Some(Duration::from_secs(3)));
    }

    #[test]
    fn test_resolve_ttl_default_finalized() {
        let config = make_config(vec![]);
        let ttl = resolve_ttl(&config, "unknown_method", false);
        assert_eq!(ttl, Some(Duration::from_secs(3600)));
    }

    #[test]
    fn test_matches_method_exact() {
        assert!(matches_method(
            &["eth_getBalance".to_string()],
            "eth_getBalance"
        ));
        assert!(!matches_method(
            &["eth_getBalance".to_string()],
            "eth_getCode"
        ));
    }

    #[test]
    fn test_matches_method_wildcard() {
        assert!(matches_method(&["*".to_string()], "eth_anything"));
    }

    #[test]
    fn test_matches_method_multiple() {
        let methods: Vec<String> = vec!["eth_getBalance".to_string(), "eth_getCode".to_string()];
        assert!(matches_method(&methods, "eth_getBalance"));
        assert!(matches_method(&methods, "eth_getCode"));
        assert!(!matches_method(&methods, "eth_getStorageAt"));
    }

    #[test]
    fn test_matches_finality_exact() {
        assert!(matches_finality(Finality::Finalized, Finality::Finalized));
        assert!(matches_finality(
            Finality::Unfinalized,
            Finality::Unfinalized
        ));
        assert!(!matches_finality(
            Finality::Finalized,
            Finality::Unfinalized
        ));
    }

    #[test]
    fn test_matches_finality_any() {
        assert!(matches_finality(Finality::Any, Finality::Finalized));
        assert!(matches_finality(Finality::Any, Finality::Unfinalized));
        assert!(matches_finality(Finality::Any, Finality::Any));
    }
}
