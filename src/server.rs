use std::sync::Arc;

use axum::{
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::cache::CacheLayer;
use crate::config::Config;
use crate::metrics as app_metrics;
use crate::rpc;
use crate::upstream::manager::ChainManager;
use crate::ws;

/// Shared application state available to all handlers.
pub struct AppState {
    pub config: Config,
    pub chain_managers: Vec<ChainManager>,
    pub cache: CacheLayer,
}

/// Build the application: connect to Redis, create chain managers, start
/// background block trackers, and return the configured axum Router.
///
/// This is separated from [`run`] so integration tests can bind their own
/// listener (e.g. port 0) without starting the metrics server.
pub async fn setup(config: Config) -> anyhow::Result<Router> {
    // Initialize metrics (idempotent)
    app_metrics::init_metrics(&config)?;

    // Initialize Redis cache
    let cache = CacheLayer::new(&config.cache).await?;
    info!("connected to Redis");

    // Initialize chain managers (upstream manager + block tracker per chain)
    let mut chain_managers = Vec::new();
    for chain_config in &config.chains {
        let cm = ChainManager::new(chain_config, cache.clone()).await?;
        info!(chain = %chain_config.name, "initialized chain manager");
        chain_managers.push(cm);
    }

    let state = Arc::new(AppState {
        config: config.clone(),
        chain_managers,
        cache,
    });

    // Start block trackers for each chain
    for (i, cm) in state.chain_managers.iter().enumerate() {
        cm.start_tracker(i, state.clone());
    }

    // Build routes dynamically based on configured chains
    let mut app = Router::new().route("/health", get(health_check));

    for (idx, chain_config) in config.chains.iter().enumerate() {
        let route = chain_config.route.clone();
        // HTTP JSON-RPC endpoint
        app = app.route(
            &route,
            post(rpc::handler::handle_rpc).with_state((state.clone(), idx)),
        );
        // WebSocket endpoint on same route
        app = app.route(
            &route,
            get(ws::handler::handle_ws).with_state((state.clone(), idx)),
        );
    }

    let app = app
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());

    Ok(app)
}

pub async fn run(config: Config) -> anyhow::Result<()> {
    let app = setup(config.clone()).await?;

    // Start metrics server on separate port
    let metrics_addr = format!(
        "{}:{}",
        config.server.metrics.address, config.server.metrics.port
    );
    let metrics_router = Router::new().route(
        &config.server.metrics.path,
        get(app_metrics::metrics_handler),
    );

    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(&metrics_addr).await.unwrap();
        info!(addr = %metrics_addr, "metrics server listening");
        axum::serve(listener, metrics_router).await.unwrap();
    });

    // Start main server
    let addr = format!("{}:{}", config.server.address, config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!(addr = %addr, "meddler listening");
    axum::serve(listener, app).await?;

    Ok(())
}

async fn health_check() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}
