use anyhow::{anyhow, Result};
use axum::{routing::get, Router};
use chrono::Utc;
use dashmap::DashMap;
use http::header::{ACCEPT, USER_AGENT};
use octocrab::OctocrabBuilder;
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::time::interval;
use tower_http::cors::CorsLayer;
use tracing::{info, warn};
use vermon::{config::Config, handlers, models::*, refresh};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize TLS crypto provider for certificate monitoring
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow!("Failed to install rustls crypto provider"))?;

    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let config = Config::default();
    info!("Starting version monitor with config: {:?}", config);

    // Setup GitHub client
    let mut builder = OctocrabBuilder::new()
        .add_header(USER_AGENT, "vermon/0.4".into())
        .add_header(ACCEPT, "application/vnd.github+json".into());

    if let Some(token) = &config.github_token {
        builder = builder.personal_token(token.clone());
        info!("Using GitHub token for authentication");
    } else {
        warn!("No GitHub token found, rate limits will apply");
    }

    let gh = builder.build()?;

    // Build network map
    let network_map: HashMap<String, String> = REPOS
        .iter()
        .flat_map(|repo| {
            let repo_key = format!("{}/{}", repo.owner, repo.name);
            repo.networks
                .iter()
                .map(move |&network| (network.to_string(), repo_key.clone()))
        })
        .collect();

    // Fetch IBP configuration
    info!("Fetching IBP network configuration...");
    let rpc_endpoints = Arc::new(DashMap::new());
    let ibp_config_map = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
    match refresh::fetch_ibp_config().await {
        Ok(ibp_config) => {
            for (network_key, network_config) in &ibp_config {
                if network_config.configuration.active == 0 {
                    continue;
                }

                let mut urls = Vec::new();
                for provider in network_config.providers.values() {
                    let dotters_urls: Vec<String> = provider
                        .rpc_urls
                        .iter()
                        .filter(|url| url.contains("dotters.network"))
                        .cloned()
                        .collect();
                    urls.extend(dotters_urls);
                }

                if !urls.is_empty() {
                    let normalized = network_key.to_lowercase();
                    rpc_endpoints.insert(normalized, urls);
                }
            }
            *ibp_config_map.write().await = ibp_config;
            info!("Loaded RPC endpoints for {} networks", rpc_endpoints.len());
        }
        Err(e) => {
            warn!("Failed to fetch IBP config: {}", e);
        }
    }

    // Fetch IBP members
    info!("Fetching IBP members...");
    let ibp_members_map = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
    match refresh::fetch_ibp_members().await {
        Ok(members) => {
            info!("Loaded {} IBP members", members.len());
            *ibp_members_map.write().await = members;
        }
        Err(e) => {
            warn!("Failed to fetch IBP members: {}", e);
        }
    }

    // Initialize application state
    let state = Arc::new(AppState {
        repos: Arc::new(DashMap::new()),
        network_map: Arc::new(network_map),
        rpc_endpoints,
        runtime_versions: Arc::new(DashMap::new()),
        client_versions: Arc::new(DashMap::new()),
        member_info: Arc::new(DashMap::new()),
        ibp_config: ibp_config_map,
        ibp_members: ibp_members_map,
        version_history: Arc::new(tokio::sync::RwLock::new(Vec::new())),
        gh,
        config: config.clone(),
        start_time: Utc::now(),
        last_refresh: Arc::new(tokio::sync::RwLock::new(None)),
        failed_repos: Arc::new(DashMap::new()),
        usage_stats: Arc::new(UsageStats::default()),
    });

    // Setup router
    let app = Router::new()
        .route("/", get(handlers::serve_ui))
        .route("/docs", get(handlers::serve_api_docs))
        .route("/style.css", get(handlers::serve_css))
        .route("/api/health", get(handlers::health))
        .route("/api/networks", get(handlers::list_networks))
        .route("/api/networks/{network}", get(handlers::get_network))
        .route("/api/repos", get(handlers::list_repos))
        .route("/api/repos/{*path}", get(handlers::get_repo))
        .route("/api/refresh", get(handlers::trigger_refresh))
        .route("/api/compare", get(handlers::compare_versions))
        .route("/api/compare/{network}", get(handlers::compare_network))
        .route("/api/members", get(handlers::list_members))
        .route("/api/members/{provider}", get(handlers::get_member))
        .route("/api/stats", get(handlers::get_usage_stats))
        .route("/api/health-stats", get(handlers::get_health_stats))
        .layer(CorsLayer::permissive())
        .with_state(state.clone());

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    info!("Server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Spawn background refresh task
    let refresh_state = state.clone();
    tokio::spawn(async move {
        info!("Starting initial data refresh...");
        refresh::refresh_all(refresh_state.clone()).await;
        info!("Initial data refresh completed");

        let mut interval = interval(config.refresh_interval);
        loop {
            interval.tick().await;
            info!("Running scheduled refresh...");
            refresh::refresh_all(refresh_state.clone()).await;
        }
    });

    axum::serve(listener, app).await?;

    Ok(())
}
