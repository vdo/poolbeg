use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub server: ServerConfig,
    pub cache: CacheConfig,
    pub chains: Vec<ChainConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    #[serde(default = "default_address")]
    pub address: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default = "default_server_max_rps")]
    pub max_rps: u32,
    #[serde(default = "default_max_body_size")]
    pub max_body_size: usize,
    #[serde(default = "default_max_batch_size")]
    pub max_batch_size: usize,
    #[serde(default = "default_max_ws_connections")]
    pub max_ws_connections: usize,
    #[serde(default = "default_blocked_methods")]
    pub blocked_methods: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AuthConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub api_keys: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetricsConfig {
    #[serde(default = "default_address")]
    pub address: String,
    #[serde(default = "default_metrics_port")]
    pub port: u16,
    #[serde(default = "default_metrics_path")]
    pub path: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            address: default_address(),
            port: default_metrics_port(),
            path: default_metrics_path(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CacheConfig {
    pub redis: RedisConfig,
    #[serde(default)]
    pub policies: Vec<CachePolicy>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RedisConfig {
    #[serde(default = "default_redis_url")]
    pub url: String,
    #[serde(default = "default_pool_size")]
    pub pool_size: usize,
    #[serde(default)]
    pub db: u8,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CachePolicy {
    pub methods: Vec<String>,
    #[serde(default = "default_finality")]
    pub finality: Finality,
    #[serde(deserialize_with = "deserialize_ttl")]
    pub ttl: TtlValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Finality {
    Finalized,
    Unfinalized,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlValue {
    /// Never cache
    Never,
    /// Cache forever (no expiry)
    Forever,
    /// Cache with specific duration
    Duration(Duration),
}

impl Serialize for TtlValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            TtlValue::Never => serializer.serialize_str("-1"),
            TtlValue::Forever => serializer.serialize_str("0"),
            TtlValue::Duration(d) => serializer.serialize_str(&format!("{}s", d.as_secs())),
        }
    }
}

fn deserialize_ttl<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<TtlValue, D::Error> {
    let s = String::deserialize(deserializer)?;
    if s == "-1" {
        return Ok(TtlValue::Never);
    }
    if s == "0" {
        return Ok(TtlValue::Forever);
    }
    parse_duration_string(&s)
        .map(TtlValue::Duration)
        .map_err(serde::de::Error::custom)
}

fn parse_duration_string(s: &str) -> Result<Duration> {
    let s = s.trim();
    if let Some(rest) = s.strip_suffix("ms") {
        let ms: u64 = rest.trim().parse().context("invalid ms value")?;
        return Ok(Duration::from_millis(ms));
    }
    if let Some(rest) = s.strip_suffix('s') {
        let secs: u64 = rest.trim().parse().context("invalid seconds value")?;
        return Ok(Duration::from_secs(secs));
    }
    if let Some(rest) = s.strip_suffix('m') {
        let mins: u64 = rest.trim().parse().context("invalid minutes value")?;
        return Ok(Duration::from_secs(mins * 60));
    }
    if let Some(rest) = s.strip_suffix('h') {
        let hours: u64 = rest.trim().parse().context("invalid hours value")?;
        return Ok(Duration::from_secs(hours * 3600));
    }
    // Try parsing as raw seconds
    let secs: u64 = s
        .parse()
        .context("invalid duration format, use e.g. '3s', '250ms', '1h'")?;
    Ok(Duration::from_secs(secs))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChainConfig {
    pub name: String,
    pub chain_id: u64,
    #[serde(deserialize_with = "deserialize_duration")]
    pub expected_block_time: Duration,
    #[serde(default = "default_finality_depth")]
    pub finality_depth: u64,
    pub route: String,
    pub upstreams: Vec<UpstreamConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UpstreamConfig {
    pub id: String,
    #[serde(default = "default_role")]
    pub role: UpstreamRole,
    pub http_url: String,
    pub ws_url: Option<String>,
    #[serde(default = "default_max_rps")]
    pub max_rps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UpstreamRole {
    Primary,
    Secondary,
    Fallback,
}

fn deserialize_duration<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Duration, D::Error> {
    let s = String::deserialize(deserializer)?;
    parse_duration_string(&s).map_err(serde::de::Error::custom)
}

fn default_address() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    8080
}
fn default_metrics_port() -> u16 {
    9090
}
fn default_metrics_path() -> String {
    "/metrics".to_string()
}
fn default_redis_url() -> String {
    "redis://localhost:6379".to_string()
}
fn default_pool_size() -> usize {
    16
}
fn default_finality() -> Finality {
    Finality::Any
}
fn default_finality_depth() -> u64 {
    64
}
fn default_role() -> UpstreamRole {
    UpstreamRole::Primary
}
fn default_max_rps() -> u32 {
    100
}
fn default_server_max_rps() -> u32 {
    1000
}
fn default_max_body_size() -> usize {
    1_048_576 // 1MB
}
fn default_max_batch_size() -> usize {
    100
}
fn default_max_ws_connections() -> usize {
    1024
}
fn default_blocked_methods() -> Vec<String> {
    vec![
        "admin_".to_string(),
        "debug_".to_string(),
        "personal_".to_string(),
        "miner_".to_string(),
        "txpool_".to_string(),
    ]
}

/// Interpolate ${VAR} and ${VAR:-default} patterns in a string with environment variable values.
fn interpolate_env_vars(input: &str) -> String {
    let re = Regex::new(r"\$\{([^}]+)\}").unwrap();
    re.replace_all(input, |caps: &regex::Captures| {
        let expr = &caps[1];
        if let Some((var_name, default_val)) = expr.split_once(":-") {
            std::env::var(var_name)
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default_val.to_string())
        } else {
            std::env::var(expr).unwrap_or_default()
        }
    })
    .to_string()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file: {}", path.display()))?;

        // Interpolate env vars before parsing
        let interpolated = interpolate_env_vars(&raw);

        let config: Config =
            serde_yaml::from_str(&interpolated).with_context(|| "failed to parse config YAML")?;

        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.chains.is_empty(),
            "at least one chain must be configured"
        );

        let mut routes = HashMap::new();
        for chain in &self.chains {
            anyhow::ensure!(
                !chain.upstreams.is_empty(),
                "chain '{}' must have at least one upstream",
                chain.name
            );
            anyhow::ensure!(
                chain.route.starts_with('/'),
                "chain '{}' route must start with '/'",
                chain.name
            );
            if let Some(existing) = routes.insert(&chain.route, &chain.name) {
                anyhow::bail!(
                    "duplicate route '{}' used by chains '{}' and '{}'",
                    chain.route,
                    existing,
                    chain.name
                );
            }
        }

        Ok(())
    }

    /// Build a map of route path -> chain config index for fast lookup.
    pub fn route_map(&self) -> HashMap<String, usize> {
        self.chains
            .iter()
            .enumerate()
            .map(|(i, c)| (c.route.clone(), i))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_parse_duration_ms() {
        assert_eq!(
            parse_duration_string("250ms").unwrap(),
            Duration::from_millis(250)
        );
    }

    #[test]
    fn test_parse_duration_seconds() {
        assert_eq!(parse_duration_string("3s").unwrap(), Duration::from_secs(3));
    }

    #[test]
    fn test_parse_duration_minutes() {
        assert_eq!(
            parse_duration_string("5m").unwrap(),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn test_parse_duration_hours() {
        assert_eq!(
            parse_duration_string("2h").unwrap(),
            Duration::from_secs(7200)
        );
    }

    #[test]
    fn test_parse_duration_raw_seconds() {
        assert_eq!(
            parse_duration_string("60").unwrap(),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn test_parse_duration_invalid() {
        assert!(parse_duration_string("abc").is_err());
    }

    #[test]
    fn test_interpolate_env_vars_simple() {
        unsafe { std::env::set_var("MEDDLER_TEST_KEY", "hello") };
        let result = interpolate_env_vars("prefix-${MEDDLER_TEST_KEY}-suffix");
        assert_eq!(result, "prefix-hello-suffix");
        unsafe { std::env::remove_var("MEDDLER_TEST_KEY") };
    }

    #[test]
    fn test_interpolate_env_vars_missing() {
        unsafe { std::env::remove_var("MEDDLER_MISSING_VAR") };
        let result = interpolate_env_vars("url/${MEDDLER_MISSING_VAR}/path");
        assert_eq!(result, "url//path");
    }

    #[test]
    fn test_interpolate_env_vars_default() {
        unsafe { std::env::remove_var("MEDDLER_UNSET_VAR") };
        let result = interpolate_env_vars("${MEDDLER_UNSET_VAR:-fallback_value}");
        assert_eq!(result, "fallback_value");
    }

    #[test]
    fn test_interpolate_env_vars_default_overridden() {
        unsafe { std::env::set_var("MEDDLER_SET_VAR", "actual") };
        let result = interpolate_env_vars("${MEDDLER_SET_VAR:-fallback}");
        assert_eq!(result, "actual");
        unsafe { std::env::remove_var("MEDDLER_SET_VAR") };
    }

    #[test]
    fn test_interpolate_env_vars_empty_uses_default() {
        unsafe { std::env::set_var("MEDDLER_EMPTY_VAR", "") };
        let result = interpolate_env_vars("${MEDDLER_EMPTY_VAR:-default}");
        assert_eq!(result, "default");
        unsafe { std::env::remove_var("MEDDLER_EMPTY_VAR") };
    }

    #[test]
    fn test_ttl_deserialize_never() {
        let yaml = r#"
methods: ["eth_call"]
ttl: "-1"
"#;
        let policy: CachePolicy = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(policy.ttl, TtlValue::Never);
    }

    #[test]
    fn test_ttl_deserialize_forever() {
        let yaml = r#"
methods: ["eth_getBlockByHash"]
ttl: "0"
"#;
        let policy: CachePolicy = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(policy.ttl, TtlValue::Forever);
    }

    #[test]
    fn test_ttl_deserialize_duration() {
        let yaml = r#"
methods: ["eth_getBalance"]
ttl: "3s"
"#;
        let policy: CachePolicy = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(policy.ttl, TtlValue::Duration(Duration::from_secs(3)));
    }

    #[test]
    fn test_upstream_role_ordering() {
        assert!(UpstreamRole::Primary < UpstreamRole::Secondary);
        assert!(UpstreamRole::Secondary < UpstreamRole::Fallback);
    }

    #[test]
    fn test_config_load_valid() {
        let yaml = r#"
server:
  address: "0.0.0.0"
  port: 8080
cache:
  redis:
    url: "redis://localhost:6379"
  policies: []
chains:
  - name: test
    chain_id: 1
    expected_block_time: "12s"
    route: /test
    upstreams:
      - id: node1
        http_url: "http://localhost:8545"
"#;
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        tmpfile.write_all(yaml.as_bytes()).unwrap();

        let config = Config::load(tmpfile.path()).unwrap();
        assert_eq!(config.chains.len(), 1);
        assert_eq!(config.chains[0].name, "test");
        assert_eq!(config.chains[0].chain_id, 1);
        assert_eq!(
            config.chains[0].expected_block_time,
            Duration::from_secs(12)
        );
    }

    #[test]
    fn test_config_validates_no_chains() {
        let yaml = r#"
server:
  address: "0.0.0.0"
  port: 8080
cache:
  redis:
    url: "redis://localhost:6379"
  policies: []
chains: []
"#;
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        tmpfile.write_all(yaml.as_bytes()).unwrap();

        let result = Config::load(tmpfile.path());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("at least one chain")
        );
    }

    #[test]
    fn test_config_validates_duplicate_routes() {
        let yaml = r#"
server:
  address: "0.0.0.0"
  port: 8080
cache:
  redis:
    url: "redis://localhost:6379"
  policies: []
chains:
  - name: chain-a
    chain_id: 1
    expected_block_time: "12s"
    route: /eth
    upstreams:
      - id: a
        http_url: "http://a:8545"
  - name: chain-b
    chain_id: 2
    expected_block_time: "12s"
    route: /eth
    upstreams:
      - id: b
        http_url: "http://b:8545"
"#;
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        tmpfile.write_all(yaml.as_bytes()).unwrap();

        let result = Config::load(tmpfile.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("duplicate route"));
    }

    #[test]
    fn test_config_validates_route_prefix() {
        let yaml = r#"
server:
  address: "0.0.0.0"
  port: 8080
cache:
  redis:
    url: "redis://localhost:6379"
  policies: []
chains:
  - name: bad
    chain_id: 1
    expected_block_time: "12s"
    route: no-slash
    upstreams:
      - id: a
        http_url: "http://a:8545"
"#;
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        tmpfile.write_all(yaml.as_bytes()).unwrap();

        let result = Config::load(tmpfile.path());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("route must start with '/'")
        );
    }
}
