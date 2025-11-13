use crate::config::{Config, RepoConfig};
use axum::{http::StatusCode, response::{IntoResponse, Response}, Json};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use octocrab::Octocrab;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use tracing::error;

//────────────────── Repository configurations
pub static REPOS: &[RepoConfig] = &[
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
pub struct UsageStats {
    pub api_calls: DashMap<String, u64>,
    pub last_updated: std::sync::RwLock<Option<DateTime<Utc>>>,
}

pub fn track_usage(stats: &UsageStats, endpoint: &str) {
    stats.api_calls.entry(endpoint.to_string()).and_modify(|e| *e += 1).or_insert(1);
    if let Ok(mut last) = stats.last_updated.write() {
        *last = Some(Utc::now());
    }
}

//────────────────── Data Models
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: String,
    pub tag: String,
    pub published_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_notes_url: Option<String>,
    pub last_checked: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
pub struct NetworkInfo {
    pub network: String,
    pub repository: String,
    #[serde(flatten)]
    pub version_info: Option<VersionInfo>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ComparisonResult {
    pub network: String,
    pub repository_version: Option<String>,
    pub repository_url: Option<String>,
    pub repository_tag: Option<String>,
    pub runtime_version: Option<String>,
    pub client_version: Option<String>,
    pub matches: bool,
    pub last_checked: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MemberInfo {
    pub network: String,
    pub ip: String,
    pub runtime_version: Option<String>,
    pub client_version: Option<String>,
    pub last_checked: DateTime<Utc>,
    pub cert_expires: Option<DateTime<Utc>>,
    pub cert_days_left: Option<i64>,
    pub latency_ms: Option<u64>,
    pub response_time_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MemberServices {
    pub provider: String,
    pub services: Vec<MemberInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VersionChange {
    pub timestamp: DateTime<Utc>,
    pub member: String,
    pub network: String,
    pub old_version: Option<String>,
    pub new_version: Option<String>,
    pub change_type: String, // "client", "runtime", "both"
}

#[derive(Clone, Debug, Serialize)]
pub struct HealthStatus {
    pub status: &'static str,
    pub last_refresh: Option<DateTime<Utc>>,
    pub total_repos: usize,
    pub successful_updates: usize,
    pub failed_updates: Vec<String>,
    pub uptime_seconds: u64,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub include_empty: bool,
}

#[derive(Serialize)]
pub struct HealthStats {
    pub total_members: usize,
    pub total_services: usize,
    pub cert_warnings: usize,
    pub cert_critical: usize,
    pub cert_expired: usize,
    pub avg_latency_ms: Option<f64>,
    pub slow_services: usize,
    pub unreachable_services: usize,
}

//────────────────── IBP Config structures
#[derive(Debug, Clone, Deserialize)]
pub struct IbpNetworkConfig {
    #[serde(rename = "Configuration")]
    pub configuration: IbpConfiguration,
    #[serde(rename = "Providers")]
    pub providers: HashMap<String, IbpProvider>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IbpConfiguration {
    #[serde(rename = "Name")]
    #[allow(dead_code)]
    pub name: String,
    #[serde(rename = "NetworkType")]
    #[allow(dead_code)]
    pub network_type: String,
    #[serde(rename = "Active")]
    pub active: u8,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IbpProvider {
    #[serde(rename = "RpcUrls")]
    pub rpc_urls: Vec<String>,
    #[serde(rename = "Ips", default)]
    pub ips: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IbpMember {
    #[serde(rename = "Details")]
    pub details: IbpMemberDetails,
    #[serde(rename = "Membership")]
    pub membership: IbpMemberMembership,
    #[serde(rename = "Service")]
    pub service: IbpMemberService,
    #[serde(rename = "ServiceAssignments")]
    pub service_assignments: HashMap<String, Vec<String>>,
    #[serde(rename = "Location")]
    pub location: IbpMemberLocation,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IbpMemberDetails {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Website")]
    pub website: String,
    #[serde(rename = "Twitter")]
    pub twitter: String,
    #[serde(rename = "Element")]
    pub element: String,
    #[serde(rename = "Logo")]
    pub logo: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IbpMemberMembership {
    #[serde(rename = "MemberLevel")]
    pub member_level: u8,
    #[serde(rename = "Joined")]
    pub joined: u64,
    #[serde(rename = "LastRankup")]
    pub last_rankup: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IbpMemberService {
    #[serde(rename = "Active")]
    pub active: u8,
    #[serde(rename = "ServiceIPv4")]
    pub service_ipv4: String,
    #[serde(rename = "ServiceIPv6")]
    pub service_ipv6: String,
    #[serde(rename = "MonitorUrl")]
    pub monitor_url: String,
    #[serde(rename = "PrometheusEndpoint")]
    pub prometheus_endpoint: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IbpMemberLocation {
    #[serde(rename = "Region")]
    pub region: String,
    #[serde(rename = "Latitude")]
    pub latitude: f64,
    #[serde(rename = "Longitude")]
    pub longitude: f64,
}

//────────────────── Error handling
#[derive(Debug)]
pub enum ApiError {
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

pub type ApiResult<T> = Result<T, ApiError>;

//────────────────── State
#[derive(Clone)]
pub struct AppState {
    pub repos: Arc<DashMap<String, VersionInfo>>,
    pub network_map: Arc<HashMap<String, String>>,
    pub rpc_endpoints: Arc<DashMap<String, Vec<String>>>,
    pub runtime_versions: Arc<DashMap<String, String>>,
    pub client_versions: Arc<DashMap<String, String>>,
    pub member_info: Arc<DashMap<String, Vec<MemberInfo>>>,
    pub usage_stats: Arc<UsageStats>,
    pub ibp_config: Arc<tokio::sync::RwLock<HashMap<String, IbpNetworkConfig>>>,
    pub ibp_members: Arc<tokio::sync::RwLock<HashMap<String, IbpMember>>>,
    pub version_history: Arc<tokio::sync::RwLock<Vec<VersionChange>>>,
    pub gh: Octocrab,
    pub config: Config,
    pub start_time: DateTime<Utc>,
    pub last_refresh: Arc<tokio::sync::RwLock<Option<DateTime<Utc>>>>,
    pub failed_repos: Arc<DashMap<String, String>>,
}
