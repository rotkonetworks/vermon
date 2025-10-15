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
        name: "ajuna-parachain",
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

//────────────────── Usage tracking
#[derive(Debug, Default)]
struct UsageStats {
    api_calls: DashMap<String, u64>,
    last_updated: std::sync::RwLock<Option<DateTime<Utc>>>,
}

fn track_usage(stats: &UsageStats, endpoint: &str) {
    stats.api_calls.entry(endpoint.to_string()).and_modify(|e| *e += 1).or_insert(1);
    if let Ok(mut last) = stats.last_updated.write() {
        *last = Some(Utc::now());
    }
}

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
    repository_url: Option<String>,
    repository_tag: Option<String>,
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
struct IbpMember {
    #[serde(rename = "Details")]
    details: IbpMemberDetails,
    #[serde(rename = "Membership")]
    membership: IbpMemberMembership,
    #[serde(rename = "Service")]
    service: IbpMemberService,
    #[serde(rename = "ServiceAssignments")]
    service_assignments: HashMap<String, Vec<String>>,
    #[serde(rename = "Location")]
    location: IbpMemberLocation,
}

#[derive(Debug, Clone, Deserialize)]
struct IbpMemberDetails {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Website")]
    website: String,
    #[serde(rename = "Twitter")]
    twitter: String,
    #[serde(rename = "Element")]
    element: String,
    #[serde(rename = "Logo")]
    logo: String,
}

#[derive(Debug, Clone, Deserialize)]
struct IbpMemberMembership {
    #[serde(rename = "MemberLevel")]
    member_level: u8,
    #[serde(rename = "Joined")]
    joined: u64,
    #[serde(rename = "LastRankup")]
    last_rankup: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct IbpMemberService {
    #[serde(rename = "Active")]
    active: u8,
    #[serde(rename = "ServiceIPv4")]
    service_ipv4: String,
    #[serde(rename = "ServiceIPv6")]
    service_ipv6: String,
    #[serde(rename = "MonitorUrl")]
    monitor_url: String,
    #[serde(rename = "PrometheusEndpoint")]
    prometheus_endpoint: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct IbpMemberLocation {
    #[serde(rename = "Region")]
    region: String,
    #[serde(rename = "Latitude")]
    latitude: f64,
    #[serde(rename = "Longitude")]
    longitude: f64,
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
    usage_stats: Arc<UsageStats>,
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
    let url = "https://raw.githubusercontent.com/ibp-network/config/refs/heads/main/members_professional.json";
    let client = reqwest::Client::new();
    let response = client.get(url).send().await?;
    let text = response.text().await?;
    let members: HashMap<String, IbpMember> = serde_json::from_str(&text)
        .context("Failed to deserialize members_professional.json")?;
    Ok(members)
}

async fn load_member_endpoints() -> HashMap<String, String> {
    match std::fs::read_to_string("member_prometheus_endpoints.json") {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
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

//────────────────── Prometheus Metrics Queries

#[derive(Debug, Deserialize)]
struct PrometheusResponse {
    status: String,
    data: PrometheusData,
}

#[derive(Debug, Deserialize)]
struct PrometheusData {
    #[serde(rename = "resultType")]
    result_type: String,
    result: Vec<PrometheusResult>,
}

#[derive(Debug, Deserialize)]
struct PrometheusResult {
    metric: HashMap<String, String>,
    value: (f64, String),
}

async fn query_prometheus_metrics(query: &str, base_url: &str) -> Result<Vec<PrometheusResult>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let url = format!("{}/api/v1/query?query={}",
                     base_url, urlencoding::encode(query));

    let response = client
        .get(&url)
        .send()
        .await
        .context("Failed to query Prometheus")?;

    let prometheus_response: PrometheusResponse = response
        .json()
        .await
        .context("Failed to parse Prometheus response")?;

    if prometheus_response.status != "success" {
        return Err(anyhow::anyhow!("Prometheus query failed"));
    }

    Ok(prometheus_response.data.result)
}

async fn fetch_node_info_prometheus(member_name: &str, network: &str, member: &IbpMember) -> Result<(String, String)> {
    let base_url = member.service.prometheus_endpoint
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No Prometheus endpoint configured for member: {}", member_name))?;
    // Query for runtime version using substrate_build_info metric
    let build_query = format!(
        r#"substrate_build_info{{name=~"{}.*", chain="{}"}}"#,
        member_name, network
    );

    let build_results = query_prometheus_metrics(&build_query, base_url).await?;

    let client_version = build_results
        .iter()
        .find_map(|result| result.metric.get("version"))
        .ok_or_else(|| anyhow::anyhow!("No version found for {} on {}", member_name, network))?
        .clone();

    // Extract runtime spec version from the client version string
    // Expected format: "polkadot-v1.15.1-78b9700ca8e"
    let runtime_version = if let Some(captures) = regex::Regex::new(r"v(\d+\.\d+\.\d+)")
        .unwrap()
        .captures(&client_version)
    {
        captures.get(1).unwrap().as_str().to_string()
    } else {
        // Fallback: try to get runtime spec version directly
        let runtime_query = format!(
            r#"substrate_runtime_spec_version{{name=~"{}.*", chain="{}"}}"#,
            member_name, network
        );

        let runtime_results = query_prometheus_metrics(&runtime_query, base_url).await?;
        runtime_results
            .first()
            .map(|result| result.value.1.clone())
            .unwrap_or_else(|| "unknown".to_string())
    };

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
        if member.service.active == 0 || member.service.service_ipv4.is_empty() {
            continue;
        }

        let mut services = Vec::new();

        // Process all service assignments for this member
        for (_category, networks) in &member.service_assignments {
            for network in networks {
                let normalized = network.to_lowercase()
                    .replace("assethub", "asset-hub")
                    .replace("bridgehub", "bridge-hub")
                    .replace("passet-hub", "asset-hub")
                    .replace("eth-", "");

                // Try Prometheus first, fallback to HTTP if needed
                match fetch_node_info_prometheus(member_id, &normalized, member).await {
                    Ok((runtime_version, client_version)) => {
                        info!("Member {} - {}: runtime={}, client={} (from Prometheus)", member_id, normalized, runtime_version, client_version);
                        services.push(MemberInfo {
                            network: normalized.clone(),
                            ip: member.service.service_ipv4.clone(),
                            runtime_version: Some(runtime_version),
                            client_version: Some(client_version),
                            last_checked: Utc::now(),
                        });
                    }
                    Err(e) => {
                        warn!("Failed to fetch member info from Prometheus for {} on {}: {}, trying HTTP fallback", member_id, normalized, e);
                        // Fallback to HTTP RPC call
                        match fetch_node_info_http(&member.service.service_ipv4, &normalized).await {
                            Ok((runtime_version, client_version)) => {
                                info!("Member {} - {}: runtime={}, client={} (from HTTP fallback)", member_id, normalized, runtime_version, client_version);
                                services.push(MemberInfo {
                                    network: normalized.clone(),
                                    ip: member.service.service_ipv4.clone(),
                                    runtime_version: Some(runtime_version),
                                    client_version: Some(client_version),
                                    last_checked: Utc::now(),
                                });
                            }
                            Err(http_e) => {
                                warn!("Failed to fetch member info from both Prometheus and HTTP for {} on {}: Prometheus: {}, HTTP: {}",
                                     member_id, normalized, e, http_e);
                                services.push(MemberInfo {
                                    network: normalized.clone(),
                                    ip: member.service.service_ipv4.clone(),
                                    runtime_version: None,
                                    client_version: None,
                                    last_checked: Utc::now(),
                                });
                            }
                        }
                    }
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
    track_usage(&state.usage_stats, "/api/networks");
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
    track_usage(&state.usage_stats, "/api/networks/{network}");
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
    track_usage(&state.usage_stats, "/api/repos");
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
    track_usage(&state.usage_stats, "/api/repos/{repo}");
    state
        .repos
        .get(&repo)
        .map(|v| Json(v.clone()))
        .ok_or_else(|| ApiError::NotFound(format!("Repository '{}' not found", repo)))
}

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthStatus> {
    track_usage(&state.usage_stats, "/health");
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
    track_usage(&state.usage_stats, "/api/refresh");
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
    track_usage(&state.usage_stats, "/api/compare");
    let mut results = Vec::new();

    for entry in state.network_map.iter() {
        let network = entry.0;
        let repo = entry.1;

        let repo_info = state.repos.get(repo.as_str());
        let repo_version = repo_info.as_ref().map(|v| v.version.clone());
        let repo_url = repo_info.as_ref().and_then(|v| v.release_notes_url.clone());
        let repo_tag = repo_info.as_ref().map(|v| v.tag.clone());
        let runtime_version = state.runtime_versions.get(network.as_str()).map(|v| v.clone());
        let client_version = state.client_versions.get(network.as_str()).map(|v| v.clone());

        let matches = match (&repo_version, &runtime_version) {
            (Some(rv), Some(cv)) => rv == cv,
            _ => false,
        };
        results.push(ComparisonResult {
            network: network.clone(),
            repository_version: repo_version,
            repository_url: repo_url,
            repository_tag: repo_tag,
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
    track_usage(&state.usage_stats, "/api/compare/{network}");
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

    let repo_info = state.repos.get(repo.as_str());
    let repo_url = repo_info.as_ref().and_then(|v| v.release_notes_url.clone());
    let repo_tag = repo_info.as_ref().map(|v| v.tag.clone());
    Ok(Json(ComparisonResult {
        network,
        repository_version: repo_version,
        repository_url: repo_url,
        repository_tag: repo_tag,
        runtime_version,
        client_version,
        matches,
        last_checked: Utc::now(),
    }))
}

async fn list_members(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<MemberServices>> {
    track_usage(&state.usage_stats, "/api/members");
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
    track_usage(&state.usage_stats, "/api/members/{provider}");
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

async fn get_usage_stats(
    State(state): State<Arc<AppState>>,
) -> Json<HashMap<String, u64>> {
    track_usage(&state.usage_stats, "/api/stats");
    Json(state.usage_stats.api_calls.iter().map(|e| (e.key().clone(), *e.value())).collect())
}

async fn serve_ui() -> impl IntoResponse {
    (
        [("content-type", "text/html")],
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<title>vermon</title>
<link rel="stylesheet" href="/style.css">
</head>
<body>
<div class="terminal">
<div class="header">vermon v0.4.1</div>
<div class="controls">
<button id="view-toggle">by network</button>
<input id="filter" type="text" placeholder="filter">
</div>
<div id="status"></div>
<div id="data"></div>
</div>
<script>
const state={view:'network',filter:'',data:{networks:[],members:[],compare:[]}};

function parseVersion(v){
if(!v)return null;
const match=v.match(/(\d+)\.(\d+)\.(\d+)/);
return match?{major:parseInt(match[1]),minor:parseInt(match[2]),patch:parseInt(match[3]),raw:v}:null;
}
function isVersionMismatch(item){
if(!item.repository_version||!item.runtime_version)return false;
return item.repository_version!==item.runtime_version;
}
function isSemverOutdated(memberVersion,networkVersion){
const mv=parseVersion(memberVersion);
const nv=parseVersion(networkVersion);
if(!mv||!nv)return false;
// Only consider outdated if member version is actually lower
// If major version differs, check which is higher
if(mv.major!==nv.major){
return mv.major<nv.major;
}
// If minor version differs, check which is higher
if(mv.minor!==nv.minor){
return mv.minor<nv.minor;
}
// If patch version differs, check which is higher
return mv.patch<nv.patch;
}

function getMajorityVersion(versions){
const validVersions=versions.filter(v=>v&&v!=='?'&&v!==null);
if(validVersions.length===0)return null;
const counts=validVersions.reduce((acc,v)=>{acc[v]=(acc[v]||0)+1;return acc},{});
return Object.keys(counts).reduce((a,b)=>counts[a]>counts[b]?a:b);
}
function isLevel6Network(networkName){
const level6Networks=['acala','ajuna','bifrost-polkadot','hydration','nexus','kilt','moonbeam','mythos','polimec','unique','xcavate'];
return level6Networks.includes(networkName.toLowerCase());
}

function renderNetworkView(){
if(!state.data.compare.length)return '<div>loading...</div>';
const filtered=state.data.compare.filter(item=>
state.filter===''||item.network.toLowerCase().includes(state.filter.toLowerCase()));

const relayChains=filtered.filter(item=>['polkadot','kusama','paseo'].includes(item.network.toLowerCase()));
const systemChains=filtered.filter(item=>item.network.toLowerCase().includes('-hub')||item.network.toLowerCase().includes('collectives')||item.network.toLowerCase().includes('coretime')||item.network.toLowerCase().includes('people')||item.network.toLowerCase().includes('encointer'));
const level6Networks=filtered.filter(item=>isLevel6Network(item.network));
const otherNetworks=filtered.filter(item=>!['polkadot','kusama','paseo'].includes(item.network.toLowerCase())&&!item.network.toLowerCase().includes('-hub')&&!item.network.toLowerCase().includes('collectives')&&!item.network.toLowerCase().includes('coretime')&&!item.network.toLowerCase().includes('people')&&!item.network.toLowerCase().includes('encointer')&&!isLevel6Network(item.network));

function renderNetworkGroup(networks,title){
if(!networks.length)return '';
const content=networks.map(item=>{
const mismatch=isVersionMismatch(item);
const networkMembers=state.data.members.filter(m=>
m.services.some(s=>s.network===item.network));
const clientVersions=networkMembers.map(m=>{
const service=m.services.find(s=>s.network===item.network);
return service?service.client_version:null;
}).filter(v=>v&&v!=='?'&&v!==null);
const majorityVersion=getMajorityVersion(clientVersions);
const members=networkMembers.map(m=>{
const service=m.services.find(s=>s.network===item.network);
const clientVer=service?service.client_version:null;
const outdated=majorityVersion&&clientVer&&isSemverOutdated(clientVer,majorityVersion);
const unreachable=!clientVer||clientVer==='?'||clientVer===null;
let status='';
let cssClass='';
if(unreachable){status=' [unreachable]';cssClass=' class="unreachable"';}
else if(outdated){status=' [!outdated]';cssClass=' class="outdated"';}
return `<span${cssClass}>${m.provider}: ${clientVer||'?'}${status}</span>`;
}).join(' | ');
const repoInfo=state.data.networks.find(n=>n.network===item.network);
const githubRepo=repoInfo?repoInfo.repository:'unknown';
return `<div class="item ${mismatch?'invalid':'valid'}">
<strong>${item.network}</strong>
<div class="versions">
github: ${githubRepo} | repo: ${item.repository_url?`<a href="${item.repository_url}" target="_blank">${item.repository_version||'?'}</a>`:item.repository_version||'?'}
runtime: ${item.runtime_version||'?'} | majority: ${majorityVersion||'?'}
</div>
${members?`<div class="members">${members}</div>`:''}
</div>`;
}).join('');
return `<div class="network-group">
<div class="group-header">${title} (${networks.length})</div>
${content}
</div>`;
}

return [
renderNetworkGroup(relayChains,'Relay Chains'),
renderNetworkGroup(systemChains,'System Chains'),
renderNetworkGroup(level6Networks,'Level 6 Parachains'),
renderNetworkGroup(otherNetworks,'Other Networks')
].filter(group=>group).join('');
}

function renderMemberView(){
if(!state.data.members.length)return '<div>loading...</div>';
const filtered=state.data.members.filter(member=>
state.filter===''||member.provider.toLowerCase().includes(state.filter.toLowerCase()));
return filtered.map(member=>{
return `<div class="member">
<strong>${member.provider}</strong>
${member.services.map(s=>{
const compItem=state.data.compare.find(c=>c.network===s.network);
const mismatch=compItem&&s.runtime_version&&compItem.runtime_version&&s.runtime_version!==compItem.runtime_version;
const unreachable=!s.runtime_version||s.runtime_version==='?'||!s.client_version||s.client_version==='?';
const networkMembers=state.data.members.filter(m=>m.services.some(srv=>srv.network===s.network));
const clientVersions=networkMembers.map(m=>{
const service=m.services.find(srv=>srv.network===s.network);
return service?service.client_version:null;
}).filter(v=>v&&v!=='?'&&v!==null);
const majorityVersion=getMajorityVersion(clientVersions);
const outdated=majorityVersion&&s.client_version&&isSemverOutdated(s.client_version,majorityVersion);
let serviceClass='service';
if(unreachable)serviceClass+=' unreachable';
else if(mismatch)serviceClass+=' invalid';
else if(outdated)serviceClass+=' outdated';
else serviceClass+=' valid';
return `<div class="${serviceClass}">
${s.network}: ${s.runtime_version||'?'} | ${s.client_version||'?'}
</div>`;
}).join('')}
</div>`;
}).join('');
}

function render(){
document.getElementById('data').innerHTML=state.view==='network'?renderNetworkView():renderMemberView();
}

async function fetchData(){
try{
const[networks,members,compare]=await Promise.all([
fetch('/api/networks').then(r=>r.json()),
fetch('/api/members').then(r=>r.json()),
fetch('/api/compare').then(r=>r.json())
]);
state.data={networks,members,compare};
render();
document.getElementById('status').textContent=`${compare.length} networks | ${members.length} members`;
}catch(e){
document.getElementById('status').textContent='error: '+e.message;
}
}

document.getElementById('view-toggle').onclick=()=>{
state.view=state.view==='network'?'member':'network';
document.getElementById('view-toggle').textContent='by '+state.view;
render();
};

document.getElementById('filter').oninput=e=>{
state.filter=e.target.value;
render();
};

fetchData();
setInterval(fetchData,30000);
</script>
</body>
</html>"#
    )
}

async fn serve_css() -> impl IntoResponse {
    (
        [("content-type", "text/css")],
        std::fs::read_to_string("/home/alice/rotko/vermon/static/style.css")
            .unwrap_or_else(|_| "*{margin:0;padding:0;box-sizing:border-box}body{background:#000;color:#0f0;font-family:monospace;font-size:12px;padding:10px}input{background:#000;border:1px solid #333;color:#0f0;font-family:monospace;font-size:12px;padding:5px;outline:none}input:focus{border-color:#0f0}ul{list-style:none;margin-top:10px}li{padding:2px 0}li.valid{color:#0f0}li.invalid{color:#f00;background:#220}li.unknown{color:#888}ul li:hover{background:#111}".to_string())
    )
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
        usage_stats: Arc::new(UsageStats::default()),
    });

    let app = Router::new()
        .route("/", get(serve_ui))
        .route("/style.css", get(serve_css))
        .route("/api/health", get(health))
        .route("/api/networks", get(list_networks))
        .route("/api/networks/{network}", get(get_network))
        .route("/api/repos", get(list_repos))
        .route("/api/repos/{*path}", get(get_repo))
        .route("/api/refresh", get(trigger_refresh))
        .route("/api/compare", get(compare_versions))
        .route("/api/compare/{network}", get(compare_network))
        .route("/api/members", get(list_members))
        .route("/api/members/{provider}", get(get_member))
        .route("/api/stats", get(get_usage_stats))
        .layer(CorsLayer::permissive())
        .with_state(state.clone());

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    info!("Server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;

    let refresh_state = state.clone();
    tokio::spawn(async move {
        // Initial refresh
        info!("Starting initial data refresh...");
        refresh_all(refresh_state.clone()).await;
        info!("Initial data refresh completed");

        // Then start interval
        let mut interval = interval(config.refresh_interval);
        loop {
            interval.tick().await;
            info!("Running scheduled refresh...");
            refresh_all(refresh_state.clone()).await;
        }
    });

    axum::serve(listener, app).await?;
    
    Ok(())
}
