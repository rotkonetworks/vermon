use anyhow::{Context as _, Result};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use http::header::{ACCEPT, USER_AGENT};
use jsonrpsee::{
    core::client::ClientT,
    ws_client::WsClientBuilder,
};
use octocrab::{Octocrab, OctocrabBuilder};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};
use tokio::time::interval;
use tracing::{error, info, warn};
use tower_http::cors::CorsLayer;

//────────────────── Configuration
#[derive(Debug, Clone)]
struct Config {
    refresh_interval: Duration,
    github_token: Option<String>,
    port: u16,
    max_retries: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            refresh_interval: Duration::from_secs(3600),
            github_token: std::env::var("GITHUB_TOKEN").ok(),
            port: std::env::var("PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(3000),
            max_retries: 3,
        }
    }
}

//────────────────── Repository configuration
#[derive(Debug, Clone)]
struct RepoConfig {
    owner: &'static str,
    name: &'static str,
    prefix: &'static str,
    networks: &'static [&'static str],
    version_pattern: Option<&'static str>,
}

static REPOS: &[RepoConfig] = &[
    RepoConfig {
        owner: "paritytech",
        name: "polkadot-sdk",
        prefix: "polkadot-stable",
        networks: &[
            "polkadot", "kusama", "westend", "paseo",
            "asset-hub-polkadot", "asset-hub-kusama", "asset-hub-westend", "asset-hub-paseo",
            "bridge-hub-polkadot", "bridge-hub-kusama", "bridge-hub-westend", "bridge-hub-paseo",
            "collectives-polkadot", "collectives-westend",
            "coretime-polkadot", "coretime-kusama", "coretime-westend", "coretime-paseo",
            "people-polkadot", "people-kusama", "people-westend", "people-paseo",
        ],
        version_pattern: Some(r"polkadot-stable(\d{4})-(\d+)"),
    },
    RepoConfig {
        owner: "encointer",
        name: "encointer-parachain",
        prefix: "",
        networks: &["encointer", "encointer-kusama"],
        version_pattern: None,
    },
    RepoConfig {
        owner: "AcalaNetwork",
        name: "Acala",
        prefix: "",
        networks: &["acala"],
        version_pattern: None,
    },
    RepoConfig {
        owner: "ajuna-network",
        name: "Ajuna",
        prefix: "",
        networks: &["ajuna"],
        version_pattern: None,
    },
    RepoConfig {
        owner: "bifrost-finance",
        name: "bifrost",
        prefix: "",
        networks: &["bifrost-polkadot"],
        version_pattern: None,
    },
    RepoConfig {
        owner: "galacticcouncil",
        name: "hydration-node",
        prefix: "v",
        networks: &["hydration"],
        version_pattern: None,
    },
    RepoConfig {
        owner: "Abstracted-Labs",
        name: "InvArch-Node",
        prefix: "",
        networks: &["invarch"],
        version_pattern: None,
    },
    RepoConfig {
        owner: "polytope-labs",
        name: "hyperbridge",
        prefix: "",
        networks: &["nexus"],
        version_pattern: None,
    },
    RepoConfig {
        owner: "KILTprotocol",
        name: "kilt-node",
        prefix: "",
        networks: &["kilt"],
        version_pattern: None,
    },
    RepoConfig {
        owner: "moonbeam-foundation",
        name: "moonbeam",
        prefix: "",
        networks: &["moonbeam"],
        version_pattern: None,
    },
    RepoConfig {
        owner: "paritytech",
        name: "project-mythical",
        prefix: "",
        networks: &["mythos"],
        version_pattern: None,
    },
    RepoConfig {
        owner: "Polimec",
        name: "polimec-node",
        prefix: "",
        networks: &["polimec"],
        version_pattern: None,
    },
    RepoConfig {
        owner: "UniqueNetwork",
        name: "unique-chain",
        prefix: "",
        networks: &["unique"],
        version_pattern: None,
    },
];

//────────────────── Models
#[derive(Clone, Debug, Serialize, Deserialize)]
struct VersionInfo {
    version: String,
    tag: String,
    published_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_notes_url: Option<String>,
    last_checked: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
struct NetworkInfo {
    network: String,
    repository: String,
    #[serde(flatten)]
    version_info: Option<VersionInfo>,
}

#[derive(Clone, Debug, Serialize)]
struct ComparisonResult {
    network: String,
    repository_version: Option<String>,
    runtime_version: Option<String>,
    client_version: Option<String>,
    matches: bool,
    last_checked: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
struct MemberInfo {
    network: String,
    ip: String,
    runtime_version: Option<String>,
    client_version: Option<String>,
    last_checked: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
struct MemberServices {
    provider: String,
    services: Vec<MemberInfo>,
}

#[derive(Clone, Debug, Serialize)]
struct HealthStatus {
    status: &'static str,
    last_refresh: Option<DateTime<Utc>>,
    total_repos: usize,
    successful_updates: usize,
    failed_updates: Vec<String>,
    uptime_seconds: u64,
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    #[serde(default)]
    include_empty: bool,
}

//────────────────── Error handling
#[derive(Debug)]
enum ApiError {
    NotFound(String),
    Internal(anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::Internal(err) => {
                error!("Internal error: {:?}", err);
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error".to_string())
            }
        };
        
        let body = Json(serde_json::json!({
            "error": message,
            "status": status.as_u16(),
        }));
        
        (status, body).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        ApiError::Internal(err)
    }
}

type ApiResult<T> = Result<T, ApiError>;

//────────────────── IBP Config structures
#[derive(Debug, Clone, Deserialize)]
struct IbpNetworkConfig {
    #[serde(rename = "Configuration")]
    configuration: IbpConfiguration,
    #[serde(rename = "Providers")]
    providers: HashMap<String, IbpProvider>,
}

#[derive(Debug, Clone, Deserialize)]
struct IbpConfiguration {
    #[serde(rename = "Name")]
    #[allow(dead_code)]
    name: String,
    #[serde(rename = "NetworkType")]
    #[allow(dead_code)]
    network_type: String,
    #[serde(rename = "Active")]
    active: u8,
}

#[derive(Debug, Clone, Deserialize)]
struct IbpProvider {
    #[serde(rename = "RpcUrls")]
    rpc_urls: Vec<String>,
    #[serde(rename = "Ips", default)]
    ips: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct IbpMembersConfig {
    members: HashMap<String, IbpMember>,
}

#[derive(Debug, Clone, Deserialize)]
struct IbpMember {
    name: String,
    #[serde(default)]
    website: Option<String>,
    #[serde(default)]
    logo: Option<String>,
    membership: String,
    #[serde(default)]
    current_level: Option<String>,
    #[serde(default)]
    active: Option<String>,
    #[serde(default)]
    level_timestamp: Option<HashMap<String, String>>,
    #[serde(default)]
    services_address: String,
    #[serde(default, deserialize_with = "deserialize_endpoints")]
    endpoints: HashMap<String, String>,
    #[serde(default)]
    monitor_url: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    latitude: Option<String>,
    #[serde(default)]
    longitude: Option<String>,
    #[serde(default)]
    payments: Option<serde_json::Value>,
}

fn deserialize_endpoints<'de, D>(deserializer: D) -> Result<HashMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Object(map) => {
            let mut result = HashMap::new();
            for (k, v) in map {
                if let serde_json::Value::String(s) = v {
                    result.insert(k, s);
                }
            }
            Ok(result)
        }
        serde_json::Value::Array(_) => {
            // Empty array case - return empty HashMap
            Ok(HashMap::new())
        }
        _ => Err(D::Error::custom("endpoints must be object or array")),
    }
}

//────────────────── State
#[derive(Clone)]
struct AppState {
    repos: Arc<DashMap<String, VersionInfo>>,
    network_map: Arc<HashMap<String, String>>,
    rpc_endpoints: Arc<DashMap<String, Vec<String>>>,
    runtime_versions: Arc<DashMap<String, String>>,
    client_versions: Arc<DashMap<String, String>>,
    member_info: Arc<DashMap<String, Vec<MemberInfo>>>,
    ibp_config: Arc<tokio::sync::RwLock<HashMap<String, IbpNetworkConfig>>>,
    ibp_members: Arc<tokio::sync::RwLock<HashMap<String, IbpMember>>>,
    gh: Octocrab,
    config: Config,
    start_time: DateTime<Utc>,
    last_refresh: Arc<tokio::sync::RwLock<Option<DateTime<Utc>>>>,
    failed_repos: Arc<DashMap<String, String>>,
}

//────────────────── GitHub helpers
#[derive(Deserialize, Debug, Clone)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    published_at: Option<DateTime<Utc>>,
    html_url: Option<String>,
}

#[derive(Deserialize, Debug)]
struct Tag {
    name: String,
}

// Enhanced version parsing with custom patterns
fn parse_version(tag: &str, prefix: &str, pattern: Option<&str>) -> Option<Version> {
    // First try custom pattern if provided
    if let Some(pattern_str) = pattern {
        if let Ok(re) = regex::Regex::new(pattern_str) {
            if let Some(caps) = re.captures(tag) {
                // For polkadot-stable pattern
                if pattern_str.contains("(\\d{4})-(\\d+)") {
                    if let (Some(year), Some(month)) = (caps.get(1), caps.get(2)) {
                        let semver_str = format!("{}.{}.0", year.as_str(), month.as_str());
                        if let Ok(v) = Version::parse(&semver_str) {
                            return Some(v);
                        }
                    }
                }
            }
        }
    }
    
    // Standard parsing
    let clean_tag = tag
        .trim_start_matches(prefix)
        .trim_start_matches('v')
        .trim_start_matches('-');
    
    // Try direct parse
    if let Ok(v) = Version::parse(clean_tag) {
        return Some(v);
    }
    
    // Try replacing common separators
    let normalized = clean_tag
        .replace('-', ".")
        .replace('_', ".");
    
    if let Ok(v) = Version::parse(&normalized) {
        return Some(v);
    }
    
    // Try to extract numbers and create version
    let parts: Vec<&str> = clean_tag.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).collect();
    match parts.len() {
        1 => Version::parse(&format!("{}.0.0", parts[0])).ok(),
        2 => Version::parse(&format!("{}.{}.0", parts[0], parts[1])).ok(),
        3.. => Version::parse(&format!("{}.{}.{}", parts[0], parts[1], parts[2])).ok(),
        _ => None,
    }
}

async fn fetch_latest_version(
    gh: &Octocrab,
    repo_config: &RepoConfig,
) -> Result<VersionInfo> {
    let repo_path = format!("{}/{}", repo_config.owner, repo_config.name);
    
    // Try to get the latest release first
    let latest_url = format!("https://api.github.com/repos/{}/releases/latest", repo_path);
    let latest_result = gh.get::<Release, _, _>(&latest_url, None::<&()>).await;
    
    match latest_result {
        Ok(release) if !release.draft => {
            if let Some(version) = parse_version(&release.tag_name, repo_config.prefix, repo_config.version_pattern) {
                return Ok(VersionInfo {
                    version: version.to_string(),
                    tag: release.tag_name,
                    published_at: release.published_at.unwrap_or_else(Utc::now),
                    release_notes_url: release.html_url,
                    last_checked: Utc::now(),
                });
            }
        }
        _ => {}
    }
    
    // Fallback to listing releases
    let releases_url = format!("https://api.github.com/repos/{}/releases?per_page=30", repo_path);
    let releases: Vec<Release> = gh.get(&releases_url, None::<&()>).await
        .context("Failed to fetch releases")?;
    
    let mut valid_releases: Vec<(Version, Release)> = releases
        .into_iter()
        .filter(|r| !r.draft && !r.prerelease)
        .filter_map(|r| {
            parse_version(&r.tag_name, repo_config.prefix, repo_config.version_pattern)
                .map(|v| (v, r))
        })
        .collect();
    
    valid_releases.sort_by(|(v1, _), (v2, _)| v2.cmp(v1));
    
    if let Some((version, release)) = valid_releases.first() {
        return Ok(VersionInfo {
            version: version.to_string(),
            tag: release.tag_name.clone(),
            published_at: release.published_at.unwrap_or_else(Utc::now),
            release_notes_url: release.html_url.clone(),
            last_checked: Utc::now(),
        });
    }
    
    // Last resort: check tags
    let tags_url = format!("https://api.github.com/repos/{}/tags?per_page=50", repo_path);
    let tags: Vec<Tag> = gh.get(&tags_url, None::<&()>).await
        .context("Failed to fetch tags")?;
    
    let mut valid_tags: Vec<(Version, Tag)> = tags
        .into_iter()
        .filter_map(|t| {
            parse_version(&t.name, repo_config.prefix, repo_config.version_pattern)
                .map(|v| (v, t))
        })
        .collect();
    
    valid_tags.sort_by(|(v1, _), (v2, _)| v2.cmp(v1));
    
    if let Some((version, tag)) = valid_tags.first() {
        return Ok(VersionInfo {
            version: version.to_string(),
            tag: tag.name.clone(),
            published_at: Utc::now(),
            release_notes_url: None,
            last_checked: Utc::now(),
        });
    }
    
    Err(anyhow::anyhow!("No valid versions found for {}", repo_path))
}

//────────────────── Refresh logic
async fn refresh_repository(
    state: Arc<AppState>,
    repo_config: &RepoConfig,
) -> Result<()> {
    let repo_key = format!("{}/{}", repo_config.owner, repo_config.name);
    
    match fetch_latest_version(&state.gh, repo_config).await {
        Ok(version_info) => {
            info!("Updated {}: {} ({})", repo_key, version_info.version, version_info.tag);
            state.repos.insert(repo_key.clone(), version_info);
            state.failed_repos.remove(&repo_key);
            Ok(())
        }
        Err(e) => {
            let error_msg = format!("Failed to update {}: {}", repo_key, e);
            warn!("{}", error_msg);
            state.failed_repos.insert(repo_key, error_msg);
            Err(e)
        }
    }
}

//────────────────── IBP Config & RPC helpers
async fn fetch_ibp_config() -> Result<HashMap<String, IbpNetworkConfig>> {
    let url = "https://raw.githubusercontent.com/ibp-network/config/refs/heads/main/services_rpc.json";
    let client = reqwest::Client::new();
    let response = client.get(url).send().await?;
    let config: HashMap<String, IbpNetworkConfig> = response.json().await?;
    Ok(config)
}

async fn fetch_ibp_members() -> Result<HashMap<String, IbpMember>> {
    let url = "https://raw.githubusercontent.com/ibp-network/config/refs/heads/main/members.json";
    let client = reqwest::Client::new();
    let response = client.get(url).send().await?;
    let text = response.text().await?;
    let config: IbpMembersConfig = serde_json::from_str(&text)
        .context("Failed to deserialize members.json")?;
    Ok(config.members)
}

async fn fetch_runtime_version(ws_url: &str) -> Result<(String, String)> {
    let client = WsClientBuilder::default()
        .connection_timeout(Duration::from_secs(10))
        .request_timeout(Duration::from_secs(10))
        .build(ws_url)
        .await
        .context("Failed to connect to RPC")?;

    // Fetch runtime version (specVersion)
    let runtime_response: serde_json::Value = client
        .request("state_getRuntimeVersion", vec![] as Vec<()>)
        .await
        .context("Failed to get runtime version")?;

    let runtime_version = runtime_response
        .get("specVersion")
        .and_then(|v| v.as_u64())
        .map(|v| v.to_string())
        .ok_or_else(|| anyhow::anyhow!("Invalid runtime version response"))?;

    // Fetch client version (system_version)
    let client_response: String = client
        .request("system_version", vec![] as Vec<()>)
        .await
        .context("Failed to get client version")?;

    Ok((runtime_version, client_response))
}

async fn fetch_node_info_http(ip: &str, network: &str) -> Result<(String, String)> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .danger_accept_invalid_certs(true)
        .build()?;

    let url = format!("https://{}:443", ip);
    let geodns_host = format!("{}.dotters.network", network);

    let response = client
        .post(&url)
        .header("Host", &geodns_host)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "state_getRuntimeVersion",
            "params": [],
            "id": 1
        }))
        .send()
        .await
        .context("Failed to send runtime version request")?;

    let runtime_response: serde_json::Value = response.json().await?;

    let runtime_version = runtime_response
        .get("result")
        .and_then(|r| r.get("specVersion"))
        .and_then(|v| v.as_u64())
        .map(|v| v.to_string())
        .ok_or_else(|| anyhow::anyhow!("Invalid runtime version response"))?;

    let response = client
        .post(&url)
        .header("Host", &geodns_host)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "system_version",
            "params": [],
            "id": 1
        }))
        .send()
        .await
        .context("Failed to send client version request")?;

    let client_response: serde_json::Value = response.json().await?;

    let client_version = client_response
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Invalid client version response"))?
        .to_string();

    Ok((runtime_version, client_version))
}



async fn refresh_chain_versions(state: Arc<AppState>) {
    info!("Refreshing on-chain runtime and client versions...");

    for entry in state.rpc_endpoints.iter() {
        let network = entry.key();
        let endpoints = entry.value();

        for endpoint in endpoints.iter() {
            match fetch_runtime_version(endpoint).await {
                Ok((runtime_version, client_version)) => {
                    info!("Runtime version for {}: {}, Client version: {}", network, runtime_version, client_version);
                    state.runtime_versions.insert(network.clone(), runtime_version);
                    state.client_versions.insert(network.clone(), client_version);
                    break; // Success, move to next network
                }
                Err(e) => {
                    warn!("Failed to fetch versions from {} for {}: {}", endpoint, network, e);
                    continue; // Try next endpoint
                }
            }
        }
    }

    info!("Chain version refresh completed");
}

async fn refresh_member_info(state: Arc<AppState>) {
    info!("Refreshing IBP member node information...");

    let ibp_members = state.ibp_members.read().await;

    for (member_id, member) in ibp_members.iter() {
        if member.services_address.is_empty() {
            continue;
        }

        let mut services = Vec::new();

        for (network_key, _endpoint) in &member.endpoints {
            let normalized = network_key.to_lowercase()
                .replace("assethub", "asset-hub")
                .replace("bridgehub", "bridge-hub");

            match fetch_node_info_http(&member.services_address, &normalized).await {
                Ok((runtime_version, client_version)) => {
                    info!("Member {} - {}: runtime={}, client={}", member_id, normalized, runtime_version, client_version);
                    services.push(MemberInfo {
                        network: normalized.clone(),
                        ip: member.services_address.clone(),
                        runtime_version: Some(runtime_version),
                        client_version: Some(client_version),
                        last_checked: Utc::now(),
                    });
                }
                Err(e) => {
                    warn!("Failed to fetch member info from {} for {}: {}", member_id, normalized, e);
                    services.push(MemberInfo {
                        network: normalized.clone(),
                        ip: member.services_address.clone(),
                        runtime_version: None,
                        client_version: None,
                        last_checked: Utc::now(),
                    });
                }
            }
        }

        if !services.is_empty() {
            state.member_info.insert(member_id.clone(), services);
        }
    }

    info!("Member info refresh completed");
}

async fn refresh_all(state: Arc<AppState>) {
    info!("Starting refresh of all repositories...");

    if state.config.github_token.is_none() {
        info!("No GitHub token found - using rate-limited sequential refresh");
        let mut success_count = 0;

        for repo_config in REPOS.iter() {
            let mut retries = 0;
            let repo_key = format!("{}/{}", repo_config.owner, repo_config.name);

            loop {
                match refresh_repository(state.clone(), repo_config).await {
                    Ok(_) => {
                        success_count += 1;
                        break;
                    }
                    Err(e) if retries < state.config.max_retries => {
                        retries += 1;
                        warn!("Retry {} for {}: {}", retries, repo_key, e);
                        tokio::time::sleep(Duration::from_secs(5 * retries as u64)).await;
                    }
                    Err(_) => break,
                }
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        info!(
            "Refresh completed: {}/{} repositories updated successfully",
            success_count,
            REPOS.len()
        );
    } else {
        let futures = REPOS.iter().map(|repo_config| {
            let state = state.clone();
            async move {
                let mut retries = 0;
                loop {
                    match refresh_repository(state.clone(), repo_config).await {
                        Ok(_) => return true,
                        Err(e) if retries < state.config.max_retries => {
                            retries += 1;
                            warn!("Retry {} for {}/{}: {}", retries, repo_config.owner, repo_config.name, e);
                            tokio::time::sleep(Duration::from_secs(5 * retries as u64)).await;
                        }
                        Err(_) => return false,
                    }
                }
            }
        });

        let results = futures::future::join_all(futures).await;
        let success_count = results.iter().filter(|&&success| success).count();

        info!(
            "Refresh completed: {}/{} repositories updated successfully",
            success_count,
            REPOS.len()
        );
    }

    *state.last_refresh.write().await = Some(Utc::now());

    // Refresh chain versions and member info
    refresh_chain_versions(state.clone()).await;
    refresh_member_info(state).await;
}

//────────────────── API Handlers
async fn list_networks(
    Query(params): Query<ListQuery>,
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<Vec<NetworkInfo>>> {
    let mut networks: Vec<NetworkInfo> = state
        .network_map
        .iter()
        .filter_map(|(network, repo)| {
            let version_info = state.repos.get(repo).map(|v| v.clone());
            
            if !params.include_empty && version_info.is_none() {
                return None;
            }
            
            Some(NetworkInfo {
                network: network.clone(),
                repository: repo.clone(),
                version_info,
            })
        })
        .collect();
    
    // Sort by network name
    networks.sort_by(|a, b| a.network.cmp(&b.network));
    
    Ok(Json(networks))
}

async fn get_network(
    Path(network): Path<String>,
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<NetworkInfo>> {
    let repo = state
        .network_map
        .get(&network)
        .ok_or_else(|| ApiError::NotFound(format!("Network '{}' not found", network)))?;
    
    let version_info = state.repos.get(repo).map(|v| v.clone());
    
    Ok(Json(NetworkInfo {
        network,
        repository: repo.clone(),
        version_info,
    }))
}

async fn list_repos(
    State(state): State<Arc<AppState>>,
) -> Json<HashMap<String, VersionInfo>> {
    Json(
        state
            .repos
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect(),
    )
}

async fn get_repo(
    Path(repo): Path<String>,
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<VersionInfo>> {
    state
        .repos
        .get(&repo)
        .map(|v| Json(v.clone()))
        .ok_or_else(|| ApiError::NotFound(format!("Repository '{}' not found", repo)))
}

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthStatus> {
    let uptime = Utc::now()
        .signed_duration_since(state.start_time)
        .num_seconds() as u64;
    
    let last_refresh = *state.last_refresh.read().await;
    
    let failed: Vec<String> = state
        .failed_repos
        .iter()
        .map(|e| e.key().clone())
        .collect();
    
    let status = if failed.is_empty() {
        "healthy"
    } else if failed.len() > REPOS.len() / 2 {
        "degraded"
    } else {
        "partial"
    };
    
    Json(HealthStatus {
        status,
        last_refresh,
        total_repos: REPOS.len(),
        successful_updates: REPOS.len() - failed.len(),
        failed_updates: failed,
        uptime_seconds: uptime,
    })
}

async fn trigger_refresh(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Check if a refresh is already in progress
    let last_refresh = *state.last_refresh.read().await;
    if let Some(last) = last_refresh {
        let elapsed = Utc::now().signed_duration_since(last).num_seconds();
        if elapsed < 60 {
            return Ok(Json(serde_json::json!({
                "status": "rate_limited",
                "message": "Refresh already performed recently",
                "next_allowed_in": 60 - elapsed,
            })));
        }
    }

    // Spawn refresh task
    tokio::spawn(refresh_all(state));

    Ok(Json(serde_json::json!({
        "status": "triggered",
        "message": "Refresh started in background",
    })))
}

async fn compare_versions(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<ComparisonResult>> {
    let mut results = Vec::new();

    for entry in state.network_map.iter() {
        let network = entry.0;
        let repo = entry.1;

        let repo_version = state.repos.get(repo.as_str()).map(|v| v.version.clone());
        let runtime_version = state.runtime_versions.get(network.as_str()).map(|v| v.clone());
        let client_version = state.client_versions.get(network.as_str()).map(|v| v.clone());

        let matches = match (&repo_version, &runtime_version) {
            (Some(rv), Some(cv)) => rv == cv,
            _ => false,
        };

        results.push(ComparisonResult {
            network: network.clone(),
            repository_version: repo_version,
            runtime_version,
            client_version,
            matches,
            last_checked: Utc::now(),
        });
    }

    results.sort_by(|a, b| a.network.cmp(&b.network));
    Json(results)
}

async fn compare_network(
    Path(network): Path<String>,
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<ComparisonResult>> {
    let repo = state
        .network_map
        .get(&network)
        .ok_or_else(|| ApiError::NotFound(format!("Network '{}' not found", network)))?;

    let repo_version = state.repos.get(repo.as_str()).map(|v| v.version.clone());
    let runtime_version = state.runtime_versions.get(&network).map(|v| v.clone());
    let client_version = state.client_versions.get(&network).map(|v| v.clone());

    let matches = match (&repo_version, &runtime_version) {
        (Some(rv), Some(cv)) => rv == cv,
        _ => false,
    };

    Ok(Json(ComparisonResult {
        network,
        repository_version: repo_version,
        runtime_version,
        client_version,
        matches,
        last_checked: Utc::now(),
    }))
}

async fn list_members(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<MemberServices>> {
    let mut results: Vec<MemberServices> = state
        .member_info
        .iter()
        .map(|entry| MemberServices {
            provider: entry.key().clone(),
            services: entry.value().clone(),
        })
        .collect();

    results.sort_by(|a, b| a.provider.cmp(&b.provider));
    Json(results)
}

async fn get_member(
    Path(member_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<MemberServices>> {
    let services = state
        .member_info
        .get(&member_id)
        .map(|v| v.clone())
        .ok_or_else(|| ApiError::NotFound(format!("Member '{}' not found", member_id)))?;

    Ok(Json(MemberServices {
        provider: member_id,
        services,
    }))
}

//────────────────── Main
#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let config = Config::default();
    info!("Starting version monitor with config: {:?}", config);

    let mut builder = OctocrabBuilder::new()
        .add_header(USER_AGENT, "vermon/0.3".into())
        .add_header(ACCEPT, "application/vnd.github+json".into());
    
    if let Some(token) = &config.github_token {
        builder = builder.personal_token(token.clone());
        info!("Using GitHub token for authentication");
    } else {
        warn!("No GitHub token found, rate limits will apply");
    }

    let gh = builder.build()?;

    let network_map: HashMap<String, String> = REPOS
        .iter()
        .flat_map(|repo| {
            let repo_key = format!("{}/{}", repo.owner, repo.name);
            repo.networks
                .iter()
                .map(move |&network| (network.to_string(), repo_key.clone()))
        })
        .collect();

    info!("Fetching IBP network configuration...");
    let rpc_endpoints = Arc::new(DashMap::new());
    let ibp_config_map = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
    match fetch_ibp_config().await {
        Ok(ibp_config) => {
            for (network_key, network_config) in &ibp_config {
                if network_config.configuration.active == 0 {
                    continue;
                }

                let mut urls = Vec::new();
                for provider in network_config.providers.values() {
                    let dotters_urls: Vec<String> = provider.rpc_urls.iter()
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

    info!("Fetching IBP members...");
    let ibp_members_map = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
    match fetch_ibp_members().await {
        Ok(members) => {
            info!("Loaded {} IBP members", members.len());
            *ibp_members_map.write().await = members;
        }
        Err(e) => {
            warn!("Failed to fetch IBP members: {}", e);
        }
    }

    let state = Arc::new(AppState {
        repos: Arc::new(DashMap::new()),
        network_map: Arc::new(network_map),
        rpc_endpoints,
        runtime_versions: Arc::new(DashMap::new()),
        client_versions: Arc::new(DashMap::new()),
        member_info: Arc::new(DashMap::new()),
        ibp_config: ibp_config_map,
        ibp_members: ibp_members_map,
        gh,
        config: config.clone(),
        start_time: Utc::now(),
        last_refresh: Arc::new(tokio::sync::RwLock::new(None)),
        failed_repos: Arc::new(DashMap::new()),
    });

    refresh_all(state.clone()).await;

    let refresh_state = state.clone();
    tokio::spawn(async move {
        let mut interval = interval(config.refresh_interval);
        loop {
            interval.tick().await;
            info!("Running scheduled refresh...");
            refresh_all(refresh_state.clone()).await;
        }
    });

    let app = Router::new()
        .route("/", get(|| async { "Version Monitor API v0.4" }))
        .route("/health", get(health))
        .route("/api/networks", get(list_networks))
        .route("/api/networks/{network}", get(get_network))
        .route("/api/repos", get(list_repos))
        .route("/api/repos/{*path}", get(get_repo))
        .route("/api/refresh", get(trigger_refresh))
        .route("/api/compare", get(compare_versions))
        .route("/api/compare/{network}", get(compare_network))
        .route("/api/members", get(list_members))
        .route("/api/members/{provider}", get(get_member))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    info!("Server listening on {}", addr);
    
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    
    Ok(())
}
