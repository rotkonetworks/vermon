use crate::models::*;
use crate::templates::{DocsTemplate, IndexTemplate};
use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::header,
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use std::{collections::HashMap, sync::Arc};

pub async fn serve_ui() -> impl IntoResponse {
    let template = IndexTemplate {
        version: env!("CARGO_PKG_VERSION"),
    };
    match template.render() {
        Ok(html) => ([(header::CONTENT_TYPE, "text/html")], html).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("Template error: {}", e),
        )
            .into_response(),
    }
}

pub async fn serve_api_docs() -> impl IntoResponse {
    let template = DocsTemplate {};
    match template.render() {
        Ok(html) => ([(header::CONTENT_TYPE, "text/html")], html).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("Template error: {}", e),
        )
            .into_response(),
    }
}

pub async fn serve_css() -> impl IntoResponse {
    (
        [("content-type", "text/css")],
        std::fs::read_to_string("static/style.css")
            .unwrap_or_else(|_| "*{margin:0;padding:0;box-sizing:border-box}body{background:#000;color:#0f0;font-family:monospace;font-size:12px;padding:10px}input{background:#000;border:1px solid #333;color:#0f0;font-family:monospace;font-size:12px;padding:5px;outline:none}input:focus{border-color:#0f0}ul{list-style:none;margin-top:10px}li{padding:2px 0}li.valid{color:#0f0}li.invalid{color:#f00;background:#220}li.unknown{color:#888}ul li:hover{background:#111}".to_string())
    )
}

pub async fn health(State(state): State<Arc<AppState>>) -> Json<HealthStatus> {
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

pub async fn list_networks(
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

pub async fn get_network(
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

pub async fn list_repos(
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

pub async fn get_repo(
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

pub async fn trigger_refresh(
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
    tokio::spawn(crate::refresh::refresh_all(state));

    Ok(Json(serde_json::json!({
        "status": "triggered",
        "message": "Refresh started in background",
    })))
}

pub async fn compare_versions(
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

pub async fn compare_network(
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

pub async fn list_members(
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

pub async fn get_member(
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

pub async fn get_usage_stats(
    State(state): State<Arc<AppState>>,
) -> Json<HashMap<String, u64>> {
    track_usage(&state.usage_stats, "/api/stats");
    Json(state.usage_stats.api_calls.iter().map(|e| (e.key().clone(), *e.value())).collect())
}

pub async fn get_health_stats(
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
