use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rand::Rng;

use crate::config::UpstreamStrategy;
use crate::upstream::client::UpstreamClient;

/// Select an upstream from a list of healthy candidates using the given strategy.
/// Returns `None` if `candidates` is empty.
pub fn select(
    candidates: &[Arc<UpstreamClient>],
    strategy: UpstreamStrategy,
    round_robin: &AtomicUsize,
) -> Option<Arc<UpstreamClient>> {
    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        return Some(candidates[0].clone());
    }

    match strategy {
        UpstreamStrategy::RoundRobin => {
            let idx = round_robin.fetch_add(1, Ordering::Relaxed) % candidates.len();
            Some(candidates[idx].clone())
        }
        UpstreamStrategy::Random => {
            let idx = rand::thread_rng().gen_range(0..candidates.len());
            Some(candidates[idx].clone())
        }
        UpstreamStrategy::LowestLatency => {
            candidates
                .iter()
                .min_by_key(|u| u.latency_us())
                .cloned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{UpstreamConfig, UpstreamRole};

    fn make_upstream(id: &str) -> Arc<UpstreamClient> {
        Arc::new(
            UpstreamClient::new(&UpstreamConfig {
                id: id.to_string(),
                role: UpstreamRole::Primary,
                http_url: "http://localhost:8545".to_string(),
                ws_url: None,
                max_rps: 100,
            }, std::time::Duration::from_secs(86400), "test".to_string())
            .unwrap(),
        )
    }

    #[test]
    fn test_select_empty() {
        let rr = AtomicUsize::new(0);
        assert!(select(&[], UpstreamStrategy::RoundRobin, &rr).is_none());
        assert!(select(&[], UpstreamStrategy::Random, &rr).is_none());
        assert!(select(&[], UpstreamStrategy::LowestLatency, &rr).is_none());
    }

    #[test]
    fn test_select_single() {
        let rr = AtomicUsize::new(0);
        let ups = vec![make_upstream("a")];
        let picked = select(&ups, UpstreamStrategy::RoundRobin, &rr).unwrap();
        assert_eq!(picked.id, "a");
    }

    #[test]
    fn test_round_robin_cycles() {
        let rr = AtomicUsize::new(0);
        let ups = vec![make_upstream("a"), make_upstream("b"), make_upstream("c")];

        let ids: Vec<String> = (0..6)
            .map(|_| select(&ups, UpstreamStrategy::RoundRobin, &rr).unwrap().id.clone())
            .collect();
        assert_eq!(ids, vec!["a", "b", "c", "a", "b", "c"]);
    }

    #[test]
    fn test_random_returns_valid() {
        let rr = AtomicUsize::new(0);
        let ups = vec![make_upstream("a"), make_upstream("b")];
        for _ in 0..20 {
            let picked = select(&ups, UpstreamStrategy::Random, &rr).unwrap();
            assert!(picked.id == "a" || picked.id == "b");
        }
    }

    #[test]
    fn test_lowest_latency_prefers_lower() {
        let rr = AtomicUsize::new(0);
        let ups = vec![make_upstream("slow"), make_upstream("fast")];
        // Simulate: "slow" has 500ms latency, "fast" has 50ms
        use std::time::Duration;
        ups[0].update_latency_for_test(Duration::from_millis(500));
        ups[1].update_latency_for_test(Duration::from_millis(50));

        let picked = select(&ups, UpstreamStrategy::LowestLatency, &rr).unwrap();
        assert_eq!(picked.id, "fast");
    }
}
