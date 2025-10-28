use anyhow::{anyhow, Context as _, Result};
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
use x509_parser::prelude::*;
use rustls::pki_types::ServerName;
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
    cert_warning_days: i64,
    cert_critical_days: i64,
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
            cert_warning_days: std::env::var("CERT_WARNING_DAYS")
                .ok()
                .and_then(|d| d.parse().ok())
                .unwrap_or(30),
            cert_critical_days: std::env::var("CERT_CRITICAL_DAYS")
                .ok()
                .and_then(|d| d.parse().ok())
                .unwrap_or(7),
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
    cert_expires: Option<DateTime<Utc>>,
    cert_days_left: Option<i64>,
    latency_ms: Option<u64>,
    response_time_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
struct MemberServices {
    provider: String,
    services: Vec<MemberInfo>,
}

#[derive(Debug, Clone, Serialize)]
struct VersionChange {
    timestamp: DateTime<Utc>,
    member: String,
    network: String,
    old_version: Option<String>,
    new_version: Option<String>,
    change_type: String, // "client", "runtime", "both"
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
    version_history: Arc<tokio::sync::RwLock<Vec<VersionChange>>>,
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
        .timeout(Duration::from_secs(3))
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

    let runtime_version = if let Ok(regex) = regex::Regex::new(r"v(\d+\.\d+\.\d+)") {
        if let Some(captures) = regex.captures(&client_version) {
            captures.get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_else(|| "unknown".to_string())
        } else {
            "unknown".to_string()
        }
    } else {
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
    let start = std::time::Instant::now();
    info!("Refreshing IBP member node information...");

    let ibp_members = state.ibp_members.read().await;

    let (prometheus_members, http_members): (Vec<_>, Vec<_>) = ibp_members
        .iter()
        .filter(|(_, member)| member.service.active != 0 && !member.service.service_ipv4.is_empty())
        .partition(|(_, member)| member.service.prometheus_endpoint.is_some());

    let mut prometheus_futures = Vec::new();
    for (member_id, member) in prometheus_members {
        let endpoint = match member.service.prometheus_endpoint.as_ref() {
            Some(ep) => ep,
            None => {
                warn!("Member {} has no Prometheus endpoint but was in prometheus_members", member_id);
                continue;
            }
        };
        let networks: Vec<_> = member.service_assignments.values()
            .flatten()
            .map(|n| n.to_lowercase()
                .replace("assethub", "asset-hub")
                .replace("bridgehub", "bridge-hub")
                .replace("passet-hub", "asset-hub")
                .replace("eth-", ""))
            .collect();

        prometheus_futures.push(fetch_member_prometheus_batch(member_id.clone(), networks, endpoint.clone()));
    }

    let mut http_futures = Vec::new();
    for (member_id, member) in http_members {
        let networks: Vec<_> = member.service_assignments.values()
            .flatten()
            .map(|n| n.to_lowercase()
                .replace("assethub", "asset-hub")
                .replace("bridgehub", "bridge-hub")
                .replace("passet-hub", "asset-hub")
                .replace("eth-", ""))
            .collect();

        http_futures.push(fetch_member_http_batch(member_id.clone(), member.service.service_ipv4.clone(), networks));
    }

    let (prometheus_results, http_results) = tokio::join!(
        futures::future::join_all(prometheus_futures),
        futures::future::join_all(http_futures)
    );

    let prom_count = prometheus_results.len();
    let http_count = http_results.len();

    for (member_id, services) in prometheus_results.into_iter().chain(http_results) {
        if !services.is_empty() {
            state.member_info.insert(member_id, services);
        }
    }

    let duration = start.elapsed();
    info!("Member info refresh completed in {:?} ({} Prometheus, {} HTTP)",
          duration, prom_count, http_count);
}

async fn fetch_member_prometheus_batch(member_id: String, networks: Vec<String>, endpoint: String) -> (String, Vec<MemberInfo>) {
    let mut services = Vec::new();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .pool_idle_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| warn!("Failed to create HTTP client: {}", e))
        .unwrap_or_else(|_| reqwest::Client::new());

    for network in networks {
        // Query for client version (substrate_build_info)
        let build_query = format!(r#"substrate_build_info{{name=~"{}.*", chain="{}"}}"#, member_id, network);
        let build_url = format!("{}/api/v1/query?query={}", endpoint, urlencoding::encode(&build_query));

        let client_version = match client.get(&build_url).send().await {
            Ok(response) => {
                if let Ok(prom_response) = response.json::<PrometheusResponse>().await {
                    if let Some(result) = prom_response.data.result.first() {
                        result.metric.get("version").cloned()
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            Err(_) => None,
        };

        // Query for runtime version (substrate_runtime_spec_version)
        let runtime_query = format!(r#"substrate_runtime_spec_version{{name=~"{}.*", chain="{}"}}"#, member_id, network);
        let runtime_url = format!("{}/api/v1/query?query={}", endpoint, urlencoding::encode(&runtime_query));

        let runtime_version = match client.get(&runtime_url).send().await {
            Ok(response) => {
                if let Ok(prom_response) = response.json::<PrometheusResponse>().await {
                    if let Some(result) = prom_response.data.result.first() {
                        Some(result.value.1.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            Err(_) => None,
        };

        if client_version.is_some() || runtime_version.is_some() {
            info!("Member {} - {}: runtime={:?}, client={:?} (from Prometheus)",
                  member_id, network, runtime_version, client_version);
            services.push(MemberInfo {
                network: network.clone(),
                ip: "prometheus".to_string(),
                runtime_version,
                client_version,
                last_checked: Utc::now(),
                cert_expires: None,
                cert_days_left: None,
                latency_ms: None,
                response_time_ms: None,
            });
        } else {
            warn!("Prometheus query failed for {} on {} - no version data found", member_id, network);
        }
    }

    (member_id, services)
}

async fn fetch_member_http_batch(member_id: String, ip: String, networks: Vec<String>) -> (String, Vec<MemberInfo>) {
    let mut services = Vec::new();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(8)
        .tcp_keepalive(Duration::from_secs(60))
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let futures: Vec<_> = networks.iter()
        .map(|network| fetch_node_info_http_optimized(&client, &ip, network))
        .collect();

    let results = futures::future::join_all(futures).await;

    // Check certificate expiry and latency once per member IP
    let (cert_expires, cert_days_left) = check_certificate_expiry(&ip).await;
    let latency_ms = measure_latency(&ip).await;

    for (network, result) in networks.into_iter().zip(results.into_iter()) {
        match result {
            Ok((runtime_version, client_version, response_time)) => {
                services.push(MemberInfo {
                    network: network.clone(),
                    ip: ip.clone(),
                    runtime_version: Some(runtime_version),
                    client_version: Some(client_version),
                    last_checked: Utc::now(),
                    cert_expires,
                    cert_days_left,
                    latency_ms,
                    response_time_ms: Some(response_time),
                });
            }
            Err(_) => {
                services.push(MemberInfo {
                    network: network.clone(),
                    ip: ip.clone(),
                    runtime_version: None,
                    client_version: None,
                    last_checked: Utc::now(),
                    cert_expires,
                    cert_days_left,
                    latency_ms,
                    response_time_ms: None,
                });
            }
        }
    }

    (member_id, services)
}

async fn fetch_node_info_http_optimized(client: &reqwest::Client, ip: &str, network: &str) -> Result<(String, String, u64)> {
    let url = format!("https://{}:443", ip);
    let geodns_host = format!("{}.dotters.network", network);

    let strategies = [
        ("Host", geodns_host.as_str()),
        ("X-Forwarded-Host", geodns_host.as_str()),
        ("X-Real-IP", "127.0.0.1"),
    ];

    let start = std::time::Instant::now();

    for (header_name, header_value) in &strategies {
        match try_rpc_call(client, &url, header_name, header_value).await {
            Ok((runtime, client_ver)) => {
                let response_time = start.elapsed().as_millis() as u64;
                return Ok((runtime, client_ver, response_time));
            },
            Err(_) => continue,
        }
    }

    Err(anyhow::anyhow!("All header strategies failed for {}", network))
}

async fn measure_latency(ip: &str) -> Option<u64> {
    use tokio::time::{timeout, Instant};

    let ping_future = async {
        let start = Instant::now();
        let addr = format!("{}:443", ip);
        match tokio::net::TcpStream::connect(&addr).await {
            Ok(_) => Some(start.elapsed().as_millis() as u64),
            Err(_) => None,
        }
    };

    match timeout(Duration::from_secs(3), ping_future).await {
        Ok(result) => result,
        Err(_) => None,
    }
}

async fn try_rpc_call(client: &reqwest::Client, url: &str, header_name: &str, header_value: &str) -> Result<(String, String)> {
    let runtime_response = client
        .post(url)
        .header(header_name, header_value)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "state_getRuntimeVersion",
            "params": [],
            "id": 1
        }))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    let runtime_version = runtime_response
        .get("result")
        .and_then(|r| r.get("specVersion"))
        .and_then(|v| v.as_u64())
        .map(|v| v.to_string())
        .ok_or_else(|| anyhow::anyhow!("Invalid runtime version response"))?;

    let client_response = client
        .post(url)
        .header(header_name, header_value)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "system_version",
            "params": [],
            "id": 1
        }))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    let client_version = client_response
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Invalid client version response"))?
        .to_string();

    Ok((runtime_version, client_version))
}

fn extract_runtime_version(version: &str) -> Option<String> {
    regex::Regex::new(r"v(\d+\.\d+\.\d+)")
        .ok()?
        .captures(version)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

async fn check_certificate_expiry(ip: &str) -> (Option<DateTime<Utc>>, Option<i64>) {
    use tokio::time::timeout;

    let check_future = async {
        let addr = format!("{}:443", ip);
        let stream = tokio::net::TcpStream::connect(&addr).await?;

        let config = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();

        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let domain = ServerName::try_from(ip.to_string())
            .map_err(|_| anyhow::anyhow!("Invalid domain"))?;

        let tls_stream = connector.connect(domain, stream).await?;

        if let Some(peer_certs) = tls_stream.get_ref().1.peer_certificates() {
            if let Some(cert_der) = peer_certs.first() {
                if let Ok((_, cert)) = parse_x509_certificate(cert_der.as_ref()) {
                    if let Some(expiry) = DateTime::from_timestamp(cert.validity().not_after.timestamp(), 0) {
                        let days_left = expiry.signed_duration_since(Utc::now()).num_days();
                        return Ok((Some(expiry), Some(days_left)));
                    }
                }
            }
        }

        Err(anyhow::anyhow!("No certificate found"))
    };

    match timeout(Duration::from_secs(5), check_future).await {
        Ok(Ok(result)) => result,
        _ => (None, None),
    }
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

#[derive(Serialize)]
struct HealthStats {
    total_members: usize,
    total_services: usize,
    cert_warnings: usize,
    cert_critical: usize,
    cert_expired: usize,
    avg_latency_ms: Option<f64>,
    slow_services: usize,
    unreachable_services: usize,
}

async fn get_health_stats(
    State(state): State<Arc<AppState>>,
) -> Json<HealthStats> {
    track_usage(&state.usage_stats, "/api/health");

    let mut total_services = 0;
    let mut cert_warnings = 0;
    let mut cert_critical = 0;
    let mut cert_expired = 0;
    let mut latencies = Vec::new();
    let mut slow_services = 0;
    let mut unreachable_services = 0;

    for member_services in state.member_info.iter() {
        for service in member_services.value().iter() {
            total_services += 1;

            // Certificate stats
            if let Some(days_left) = service.cert_days_left {
                if days_left < 0 {
                    cert_expired += 1;
                } else if days_left < state.config.cert_critical_days {
                    cert_critical += 1;
                } else if days_left < state.config.cert_warning_days {
                    cert_warnings += 1;
                }
            }

            // Latency stats
            if let Some(latency) = service.latency_ms {
                latencies.push(latency as f64);
                if latency > 1000 {
                    slow_services += 1;
                }
            }

            // Unreachable check
            if service.runtime_version.is_none() || service.client_version.is_none() {
                unreachable_services += 1;
            }
        }
    }

    let avg_latency = if !latencies.is_empty() {
        Some(latencies.iter().sum::<f64>() / latencies.len() as f64)
    } else {
        None
    };

    Json(HealthStats {
        total_members: state.member_info.len(),
        total_services,
        cert_warnings,
        cert_critical,
        cert_expired,
        avg_latency_ms: avg_latency,
        slow_services,
        unreachable_services,
    })
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
<div class="header">vermon v0.4.2 | <a href="/docs" style="color:#0ff;text-decoration:none">API Docs</a></div>
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
const certDays=service?service.cert_days_left:null;
const latency=service?service.latency_ms:null;
let status='';
let cssClass='';
if(unreachable){status=' [unreachable]';cssClass=' class="unreachable"';}
else if(outdated){status=' [!outdated]';cssClass=' class="outdated"';}
if(certDays!==null&&certDays!==undefined){
if(certDays<0)status+=' [CERT EXPIRED]';
else if(certDays<7)status+=` [CERT ${certDays}d CRITICAL]`;
else if(certDays<30)status+=` [CERT ${certDays}d]`;
}
if(latency!==null&&latency!==undefined){
status+=latency>1000?' [SLOW]':latency>500?' [OK]':' [FAST]';
}
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
const certStatus=s.cert_days_left!==null&&s.cert_days_left!==undefined?
(s.cert_days_left<0?' <span class="cert-expired">[CERT EXPIRED]</span>':
s.cert_days_left<7?' <span class="cert-expired">[CERT '+s.cert_days_left+'d]</span>':
s.cert_days_left<30?' <span class="cert-warning">[CERT '+s.cert_days_left+'d]</span>':''):'';
const latencyStatus=s.latency_ms!==null&&s.latency_ms!==undefined?
(s.latency_ms>1000?' <span class="latency-timeout">['+s.latency_ms+'ms]</span>':
s.latency_ms>500?' <span class="latency-slow">['+s.latency_ms+'ms]</span>':
' <span class="latency-fast">['+s.latency_ms+'ms]</span>'):'';
return `<div class="${serviceClass}">
${s.network}: ${s.runtime_version||'?'} | ${s.client_version||'?'}${certStatus}${latencyStatus}
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
        std::fs::read_to_string("static/style.css")
            .unwrap_or_else(|_| "*{margin:0;padding:0;box-sizing:border-box}body{background:#000;color:#0f0;font-family:monospace;font-size:12px;padding:10px}input{background:#000;border:1px solid #333;color:#0f0;font-family:monospace;font-size:12px;padding:5px;outline:none}input:focus{border-color:#0f0}ul{list-style:none;margin-top:10px}li{padding:2px 0}li.valid{color:#0f0}li.invalid{color:#f00;background:#220}li.unknown{color:#888}ul li:hover{background:#111}".to_string())
    )
}

async fn serve_api_docs() -> impl IntoResponse {
    (
        [("content-type", "text/html")],
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<title>vermon API Documentation</title>
<link rel="stylesheet" href="/style.css">
<style>
body{background:#000;color:#0f0;font-family:monospace;font-size:14px;padding:20px;max-width:1200px;margin:0 auto}
h1,h2{color:#0f0;margin:20px 0 10px}
.endpoint{background:#111;border-left:3px solid #0f0;padding:15px;margin:15px 0;border-radius:3px}
.method{color:#ff0;font-weight:bold}
.path{color:#0ff}
.description{color:#aaa;margin:5px 0}
a{color:#0f0;text-decoration:none}
a:hover{text-decoration:underline}
.section{margin:30px 0}
.note{background:#220;border-left:3px solid #f80;padding:10px;margin:10px 0}
</style>
</head>
<body>
<h1>vermon API Documentation</h1>
<p>Node version monitoring for blockchain networks</p>

<div class="note">
<strong>Purpose:</strong> This API tracks blockchain node versions across networks and IBP members.
The runtime version indicates blockchain protocol compatibility - nodes with different runtime versions may not be compatible with the network.
</div>

<div class="section">
<h2>Version Monitoring</h2>

<div class="endpoint">
<div><span class="method">GET</span> <span class="path"><a href="/api/compare">/api/compare</a></span></div>
<div class="description">Compare repository versions with on-chain versions for all networks. Shows version mismatches.</div>
</div>

<div class="endpoint">
<div><span class="method">GET</span> <span class="path"><a href="/api/compare/polkadot">/api/compare/{network}</a></span></div>
<div class="description">Compare versions for a specific network (e.g., polkadot, kusama, asset-hub-polkadot)</div>
</div>

<div class="endpoint">
<div><span class="method">GET</span> <span class="path"><a href="/api/networks">/api/networks</a></span></div>
<div class="description">List all monitored networks with their version info. Use ?include_empty=true to include networks without data.</div>
</div>

<div class="endpoint">
<div><span class="method">GET</span> <span class="path"><a href="/api/networks/polkadot">/api/networks/{network}</a></span></div>
<div class="description">Get version information for a specific network</div>
</div>
</div>

<div class="section">
<h2>IBP Members</h2>

<div class="endpoint">
<div><span class="method">GET</span> <span class="path"><a href="/api/members">/api/members</a></span></div>
<div class="description">List all IBP members with their node versions, certificate status, and latency</div>
</div>

<div class="endpoint">
<div><span class="method">GET</span> <span class="path"><a href="/api/members/rotko">/api/members/{provider}</a></span></div>
<div class="description">Get detailed information for a specific IBP member</div>
</div>
</div>

<div class="section">
<h2>Repository Info</h2>

<div class="endpoint">
<div><span class="method">GET</span> <span class="path"><a href="/api/repos">/api/repos</a></span></div>
<div class="description">List all monitored GitHub repositories and their latest release versions</div>
</div>

<div class="endpoint">
<div><span class="method">GET</span> <span class="path"><a href="/api/repos/paritytech/polkadot-sdk">/api/repos/{owner}/{name}</a></span></div>
<div class="description">Get latest release info for a specific repository</div>
</div>
</div>

<div class="section">
<h2>Health & Stats</h2>

<div class="endpoint">
<div><span class="method">GET</span> <span class="path"><a href="/api/health">/api/health</a></span></div>
<div class="description">System health status and uptime</div>
</div>

<div class="endpoint">
<div><span class="method">GET</span> <span class="path"><a href="/api/health-stats">/api/health-stats</a></span></div>
<div class="description">Aggregated health statistics across all members (cert warnings, latency, unreachable nodes)</div>
</div>

<div class="endpoint">
<div><span class="method">GET</span> <span class="path"><a href="/api/stats">/api/stats</a></span></div>
<div class="description">API usage statistics</div>
</div>

<div class="endpoint">
<div><span class="method">GET</span> <span class="path">/api/refresh</span></div>
<div class="description">Trigger manual data refresh (rate limited to once per minute)</div>
</div>
</div>

<div class="section">
<h2>Response Examples</h2>

<h3>Version Comparison</h3>
<pre style="background:#111;padding:10px;overflow-x:auto">{
  "network": "polkadot",
  "repository_version": "2024.09.1",
  "repository_tag": "polkadot-stable2509-1",
  "runtime_version": "1007001",
  "client_version": "1.20.1-bd19559e1fa",
  "matches": false,
  "last_checked": "2025-10-27T23:50:00Z"
}</pre>

<h3>Member Info</h3>
<pre style="background:#111;padding:10px;overflow-x:auto">{
  "provider": "rotko",
  "services": [{
    "network": "polkadot",
    "ip": "prometheus",
    "runtime_version": "1007001",
    "client_version": "1.20.1-bd19559e1fa",
    "cert_days_left": 45,
    "latency_ms": 23
  }]
}</pre>
</div>

<p style="margin-top:40px"><a href="/">← Back to Dashboard</a></p>
</body>
</html>"#
    )
}

//────────────────── Main
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
        version_history: Arc::new(tokio::sync::RwLock::new(Vec::new())),
        gh,
        config: config.clone(),
        start_time: Utc::now(),
        last_refresh: Arc::new(tokio::sync::RwLock::new(None)),
        failed_repos: Arc::new(DashMap::new()),
        usage_stats: Arc::new(UsageStats::default()),
    });

    let app = Router::new()
        .route("/", get(serve_ui))
        .route("/docs", get(serve_api_docs))
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
        .route("/api/health-stats", get(get_health_stats))
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
