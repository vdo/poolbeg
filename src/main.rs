use clap::Parser;
use std::path::PathBuf;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use poolbeg::config;
use poolbeg::server;

#[derive(Parser, Debug)]
#[command(name = "poolbeg", about = "Web3 JSON-RPC caching proxy")]
struct Cli {
    /// Path to the configuration file
    #[arg(short, long, default_value = "config.yaml")]
    config: PathBuf,

    /// Log level (trace, debug, info, warn, error)
    #[arg(short, long, default_value = "info")]
    log_level: String,

    /// Output logs as JSON
    #[arg(long, default_value_t = false)]
    json_logs: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Install rustls crypto provider before any TLS operations.
    // Both ring and aws-lc-rs get pulled in transitively, so we must pick one explicitly.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let cli = Cli::parse();

    // Initialize tracing
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        // Set external crates to warn, only poolbeg uses the requested level
        EnvFilter::new(format!("warn,poolbeg={}", cli.log_level))
    });

    if cli.json_logs {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer().json())
            .init();
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer().compact())
            .init();
    }

    info!("poolbeg v{}", env!("CARGO_PKG_VERSION"));

    // Load config
    let mut config = config::Config::load(&cli.config)?;
    info!(chains = config.chains.len(), "configuration loaded");

    // Resolve auto_public chains by fetching public RPC endpoints
    poolbeg::upstream::public_endpoints::resolve_auto_public(&mut config.chains).await?;

    for chain in &config.chains {
        info!(
            name = %chain.name,
            chain_id = chain.chain_id,
            upstreams = chain.upstreams.len(),
            route = %chain.route,
            "chain configured"
        );
    }

    // Run the server
    server::run(config).await
}
