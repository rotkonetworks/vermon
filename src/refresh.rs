use crate::config::RepoConfig;
use crate::models::*;
use anyhow::{anyhow, Context as _, Result};
use chrono::{DateTime, Utc};
use jsonrpsee::{core::client::ClientT, ws_client::WsClientBuilder};
use octocrab::Octocrab;
use rustls::pki_types::ServerName;
use semver::Version;
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tracing::{info, warn};
use x509_parser::prelude::*;

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
    let normalized = clean_tag.replace('-', ".").replace('_', ".");

    if let Ok(v) = Version::parse(&normalized) {
        return Some(v);
    }

    // Try to extract numbers and create version
    let parts: Vec<&str> = clean_tag
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .collect();
    match parts.len() {
        1 => Version::parse(&format!("{}.0.0", parts[0])).ok(),
        2 => Version::parse(&format!("{}.{}.0", parts[0], parts[1])).ok(),
        3.. => Version::parse(&format!("{}.{}.{}", parts[0], parts[1], parts[2])).ok(),
        _ => None,
    }
}

async fn fetch_latest_version(gh: &Octocrab, repo_config: &RepoConfig) -> Result<VersionInfo> {
    let repo_path = format!("{}/{}", repo_config.owner, repo_config.name);

    // Try to get the latest release first
    let latest_url = format!("https://api.github.com/repos/{}/releases/latest", repo_path);
    let latest_result = gh.get::<Release, _, _>(&latest_url, None::<&()>).await;

    match latest_result {
        Ok(release) if !release.draft => {
            if let Some(version) =
                parse_version(&release.tag_name, repo_config.prefix, repo_config.version_pattern)
            {
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
    let releases_url = format!(
        "https://api.github.com/repos/{}/releases?per_page=30",
        repo_path
    );
    let releases: Vec<Release> = gh
        .get(&releases_url, None::<&()>)
        .await
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
    let tags: Vec<Tag> = gh
        .get(&tags_url, None::<&()>)
        .await
        .context("Failed to fetch tags")?;

    let mut valid_tags: Vec<(Version, Tag)> = tags
        .into_iter()
        .filter_map(|t| {
            parse_version(&t.name, repo_config.prefix, repo_config.version_pattern).map(|v| (v, t))
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

async fn refresh_repository(state: Arc<AppState>, repo_config: &RepoConfig) -> Result<()> {
    let repo_key = format!("{}/{}", repo_config.owner, repo_config.name);

    match fetch_latest_version(&state.gh, repo_config).await {
        Ok(version_info) => {
            info!(
                "Updated {}: {} ({})",
                repo_key, version_info.version, version_info.tag
            );
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
pub async fn fetch_ibp_config() -> Result<HashMap<String, IbpNetworkConfig>> {
    let url = "https://raw.githubusercontent.com/ibp-network/config/refs/heads/main/services_rpc.json";
    let client = reqwest::Client::new();
    let response = client.get(url).send().await?;
    let config: HashMap<String, IbpNetworkConfig> = response.json().await?;
    Ok(config)
}

pub async fn fetch_ibp_members() -> Result<HashMap<String, IbpMember>> {
    let url = "https://raw.githubusercontent.com/ibp-network/config/refs/heads/main/members_professional.json";
    let client = reqwest::Client::new();
    let response = client.get(url).send().await?;
    let text = response.text().await?;
    let members: HashMap<String, IbpMember> =
        serde_json::from_str(&text).context("Failed to deserialize members_professional.json")?;
    Ok(members)
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

    let url = format!(
        "{}/api/v1/query?query={}",
        base_url,
        urlencoding::encode(query)
    );

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

async fn refresh_chain_versions(state: Arc<AppState>) {
    info!("Refreshing on-chain runtime and client versions...");

    let mut futures = Vec::new();

    for entry in state.rpc_endpoints.iter() {
        let network = entry.key().clone();
        let endpoints = entry.value().clone();
        let state_clone = state.clone();

        futures.push(async move {
            for endpoint in endpoints.iter() {
                match fetch_runtime_version(endpoint).await {
                    Ok((runtime_version, client_version)) => {
                        info!(
                            "Runtime version for {}: {}, Client version: {}",
                            network, runtime_version, client_version
                        );
                        state_clone
                            .runtime_versions
                            .insert(network.clone(), runtime_version);
                        state_clone
                            .client_versions
                            .insert(network.clone(), client_version);
                        break; // Success, move to next network
                    }
                    Err(e) => {
                        warn!(
                            "Failed to fetch versions from {} for {}: {}",
                            endpoint, network, e
                        );
                        continue; // Try next endpoint
                    }
                }
            }
        });
    }

    futures::future::join_all(futures).await;

    info!("Chain version refresh completed");
}

async fn refresh_member_info(state: Arc<AppState>) {
    let start = std::time::Instant::now();
    info!("Refreshing IBP member node information...");

    let ibp_members = state.ibp_members.read().await;

    // Use HTTP/RPC for all members to get real user-facing data
    // Prometheus only shows backend nodes, not what users see through HAProxy
    let http_members: Vec<_> = ibp_members
        .iter()
        .filter(|(_, member)| {
            member.service.active != 0 && !member.service.service_ipv4.is_empty()
        })
        .collect();

    let prometheus_members: Vec<(&String, &IbpMember)> = vec![]; // Disabled for now

    let mut prometheus_futures = Vec::new();
    for (member_id, member) in prometheus_members {
        let endpoint = match member.service.prometheus_endpoint.as_ref() {
            Some(ep) => ep,
            None => {
                warn!(
                    "Member {} has no Prometheus endpoint but was in prometheus_members",
                    member_id
                );
                continue;
            }
        };
        let networks: Vec<_> = member
            .service_assignments
            .values()
            .flatten()
            .map(|n| {
                n.to_lowercase()
                    .replace("assethub", "asset-hub")
                    .replace("bridgehub", "bridge-hub")
                    .replace("passet-hub", "asset-hub")
                    .replace("eth-", "")
            })
            .collect();

        prometheus_futures.push(fetch_member_prometheus_batch(
            member_id.clone(),
            networks,
            endpoint.clone(),
        ));
    }

    let mut http_futures = Vec::new();
    for (member_id, member) in http_members {
        let networks: Vec<_> = member
            .service_assignments
            .values()
            .flatten()
            .map(|n| {
                n.to_lowercase()
                    .replace("assethub", "asset-hub")
                    .replace("bridgehub", "bridge-hub")
                    .replace("passet-hub", "asset-hub")
                    .replace("eth-", "")
            })
            .collect();

        http_futures.push(fetch_member_http_batch(
            member_id.clone(),
            member.service.service_ipv4.clone(),
            networks,
        ));
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
    info!(
        "Member info refresh completed in {:?} ({} Prometheus, {} HTTP)",
        duration, prom_count, http_count
    );
}

async fn fetch_member_prometheus_batch(
    member_id: String,
    networks: Vec<String>,
    endpoint: String,
) -> (String, Vec<MemberInfo>) {
    let mut services = Vec::new();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .pool_idle_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| warn!("Failed to create HTTP client: {}", e))
        .unwrap_or_else(|_| reqwest::Client::new());

    for network in networks {
        // Query for client version (substrate_build_info)
        let build_query = format!(
            r#"substrate_build_info{{name=~"{}.*", chain="{}"}}"#,
            member_id, network
        );
        let build_url = format!(
            "{}/api/v1/query?query={}",
            endpoint,
            urlencoding::encode(&build_query)
        );

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
        let runtime_query = format!(
            r#"substrate_runtime_spec_version{{name=~"{}.*", chain="{}"}}"#,
            member_id, network
        );
        let runtime_url = format!(
            "{}/api/v1/query?query={}",
            endpoint,
            urlencoding::encode(&runtime_query)
        );

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
            info!(
                "Member {} - {}: runtime={:?}, client={:?} (from Prometheus)",
                member_id, network, runtime_version, client_version
            );
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
            warn!(
                "Prometheus query failed for {} on {} - no version data found",
                member_id, network
            );
        }
    }

    (member_id, services)
}

async fn fetch_member_http_batch(
    member_id: String,
    ip: String,
    networks: Vec<String>,
) -> (String, Vec<MemberInfo>) {
    let mut services = Vec::new();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(8)
        .tcp_keepalive(Duration::from_secs(60))
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let futures: Vec<_> = networks
        .iter()
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

async fn fetch_node_info_http_optimized(
    client: &reqwest::Client,
    ip: &str,
    network: &str,
) -> Result<(String, String, u64)> {
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
            }
            Err(_) => continue,
        }
    }

    Err(anyhow::anyhow!(
        "All header strategies failed for {}",
        network
    ))
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

async fn try_rpc_call(
    client: &reqwest::Client,
    url: &str,
    header_name: &str,
    header_value: &str,
) -> Result<(String, String)> {
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

async fn check_certificate_expiry(ip: &str) -> (Option<DateTime<Utc>>, Option<i64>) {
    use tokio::time::timeout;

    let check_future = async {
        let addr = format!("{}:443", ip);
        let stream = tokio::net::TcpStream::connect(&addr).await?;

        let config = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();

        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let domain =
            ServerName::try_from(ip.to_string()).map_err(|_| anyhow!("Invalid domain"))?;

        let tls_stream = connector.connect(domain, stream).await?;

        if let Some(peer_certs) = tls_stream.get_ref().1.peer_certificates() {
            if let Some(cert_der) = peer_certs.first() {
                if let Ok((_, cert)) = parse_x509_certificate(cert_der.as_ref()) {
                    if let Some(expiry) =
                        DateTime::from_timestamp(cert.validity().not_after.timestamp(), 0)
                    {
                        let days_left = expiry.signed_duration_since(Utc::now()).num_days();
                        return Ok((Some(expiry), Some(days_left)));
                    }
                }
            }
        }

        Err(anyhow!("No certificate found"))
    };

    match timeout(Duration::from_secs(5), check_future).await {
        Ok(Ok(result)) => result,
        _ => (None, None),
    }
}

pub async fn refresh_all(state: Arc<AppState>) {
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
                            warn!(
                                "Retry {} for {}/{}: {}",
                                retries, repo_config.owner, repo_config.name, e
                            );
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
