use anyhow::{Context as _, Result};
use axum::{extract::{Path, State}, routing::get, Json, Router};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use http::header::{ACCEPT, USER_AGENT};
use octocrab::{Octocrab, OctocrabBuilder};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::time::interval;

//────────────────── static mapping (updated with correct repos)
static REPOS: &[(&str, &str, &[&str])] = &[
    // Polkadot SDK - contains relay chains and many system parachains
    ("paritytech/polkadot-sdk", "polkadot-stable", &[
        // Relay chains
        "polkadot", "kusama", "westend", "paseo",
        
        // System parachains for Polkadot
        "bridge-hub-polkadot", "collectives-polkadot", "coretime-polkadot", "people-polkadot",
        
        // System parachains for Kusama
        "bridge-hub-kusama", "coretime-kusama", "people-kusama",
        
        // System parachains for Westend
        "westend", "bridge-hub-westend", "collectives-westend", "coretime-westend", "people-westend",
        
        // System parachains for Paseo
        "paseo", "bridge-hub-paseo", "coretime-paseo", "people-paseo",
    ]),
    
    // Assets Hubs with separate repos
    ("paritytech/statemint", "", &["asset-hub-polkadot"]),
    ("paritytech/statemine", "", &["asset-hub-kusama"]),
    ("paritytech/polkadot-sdk", "polkadot-stable", &["asset-hub-westend", "asset-hub-paseo"]),
    
    // Community Parachains
    ("encointer/encointer-parachain", "", &["encointer", "encointer-kusama"]),
    ("AcalaNetwork/Acala", "", &["acala"]),
    ("ajuna-network/Ajuna", "", &["ajuna"]),
    ("bifrost-finance/bifrost", "", &["bifrost-polkadot"]),
    ("galacticcouncil/hydration-node", "v", &["hydration"]),
    ("Abstracted-Labs/InvArch-Node", "", &["invarch"]),
    ("polytope-labs/hyperbridge", "", &["nexus"]),
    ("KILTprotocol/kilt-node", "", &["kilt"]),
    ("moonbeam-foundation/moonbeam", "", &["moonbeam"]),
    ("paritytech/project-mythical", "", &["mythos"]),
    ("Polimec/polimec-node", "", &["polimec"]),
    ("UniqueNetwork/unique-chain", "", &["unique"]),
];

//────────────────── models
#[derive(Clone, Debug, Serialize)]
struct VersionInfo { version: String, tag: String, ts: DateTime<Utc> }

#[derive(Clone)]
struct Ctx {
    map: Arc<DashMap<&'static str, VersionInfo>>,
    net: Arc<HashMap<&'static str, &'static str>>,
    gh : Octocrab,
}

//────────────────── GitHub helper
#[derive(Deserialize, Debug, Clone)]
struct Release { 
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    published_at: Option<DateTime<Utc>>,
}

// Parse semver from a tag, handling prefixes and dashes
fn parse_version(tag: &str, prefix: &str) -> Option<Version> {
    let clean_tag = tag.trim_start_matches(prefix)
                      .trim_start_matches('v')
                      .replace('-', ".");
    
    // Try to parse as semver
    if let Ok(v) = Version::parse(&clean_tag) {
        return Some(v);
    }
    
    // Some projects use different formats, try to handle common cases
    // For polkadot-stable, we might have format like "polkadot-stable2023-5"
    if tag.starts_with("polkadot-stable") {
        if let Some(year_start) = tag.find(|c: char| c.is_ascii_digit()) {
            let year_part = &tag[year_start..];
            // Try to extract year and month, e.g., "2023-5" -> "2023.5.0"
            if let Some(dash_pos) = year_part.find('-') {
                let year = &year_part[..dash_pos];
                let month = &year_part[dash_pos+1..];
                let semver_str = format!("{}.{}.0", year, month);
                if let Ok(v) = Version::parse(&semver_str) {
                    return Some(v);
                }
            } else {
                // Just year, e.g., "2023" -> "2023.0.0"
                let semver_str = format!("{}.0.0", year_part);
                if let Ok(v) = Version::parse(&semver_str) {
                    return Some(v);
                }
            }
        }
    }
    
    None
}

async fn latest(gh: &Octocrab, repo: &str, pfx: &str) -> Result<(Version, String)> {
    // First try getting the latest release
    let latest_url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let latest_release_result = gh.get::<Release, _, _>(&latest_url, None::<&()>).await;
    
    match latest_release_result {
        Ok(release) => {
            if let Some(version) = parse_version(&release.tag_name, pfx) {
                return Ok((version, release.tag_name));
            }
        },
        Err(e) => {
            println!("Failed to get latest release for {repo}: {e}");
            // Continue to fallback method
        }
    }
    
    // Fallback: get list of releases and find the highest version
    let releases_url = format!("https://api.github.com/repos/{repo}/releases?per_page=20");
    let releases: Vec<Release> = gh.get(&releases_url, None::<&()>).await?;
    
    if releases.is_empty() {
        return Err(anyhow::anyhow!("No releases found for {repo}"));
    }
    
    // Filter out drafts and prereleases, then sort by version
    let mut parsed_releases: Vec<(Version, String, Option<DateTime<Utc>>)> = releases
        .iter()
        .filter(|r| !r.draft && !r.prerelease)
        .filter_map(|r| {
            parse_version(&r.tag_name, pfx).map(|v| (v, r.tag_name.clone(), r.published_at))
        })
        .collect();
    
    // Sort by version, highest first
    parsed_releases.sort_by(|(v1, _, _), (v2, _, _)| v2.cmp(v1));
    
    // If we have releases with versions, return the highest one
    if let Some((version, tag, _)) = parsed_releases.first() {
        return Ok((version.clone(), tag.clone()));
    }
    
    // Still no valid version found, try again but including prereleases
    let mut all_releases: Vec<(Version, String)> = releases
        .iter()
        .filter_map(|r| {
            parse_version(&r.tag_name, pfx).map(|v| (v, r.tag_name.clone()))
        })
        .collect();
    
    // Sort by version, highest first
    all_releases.sort_by(|(v1, _), (v2, _)| v2.cmp(v1));
    
    // Return the highest version found, even if it's a prerelease
    if let Some((version, tag)) = all_releases.first() {
        return Ok((version.clone(), tag.clone()));
    }
    
    // If still nothing found, check for tags directly
    let tags_url = format!("https://api.github.com/repos/{repo}/tags?per_page=100");
    let tags: Vec<serde_json::Value> = gh.get(&tags_url, None::<&()>).await?;
    
    #[derive(Debug, Clone)]
    struct TagInfo {
        name: String,
        version: Version,
    }
    
    let mut parsed_tags: Vec<TagInfo> = Vec::new();
    for tag in tags {
        if let Some(name) = tag["name"].as_str() {
            if let Some(version) = parse_version(name, pfx) {
                parsed_tags.push(TagInfo { name: name.to_string(), version });
            }
        }
    }
    
    // Sort by version, highest first
    parsed_tags.sort_by(|a, b| b.version.cmp(&a.version));
    
    // Return the highest version found
    if let Some(tag) = parsed_tags.first() {
        return Ok((tag.version.clone(), tag.name.clone()));
    }
    
    Err(anyhow::anyhow!("No parseable releases or tags found for {repo}"))
}

//────────────────── cache refresh
async fn refresh(ctx: Arc<Ctx>) {
    println!("Starting refresh...");
    let futs = REPOS.iter().map(|(repo,pfx,_)| {
        let ctx = ctx.clone();
        let repo_str = *repo; // Copy the string reference
        async move {
            let gh = ctx.gh.clone();
            match latest(&gh, repo_str, pfx).await {
                Ok((v, tag)) => {
                    println!("Updated {}: {} ({})", repo_str, v, tag);
                    ctx.map.insert(repo_str, VersionInfo {
                        version: v.to_string(),
                        tag,
                        ts: Utc::now(),
                    });
                    true
                },
                Err(e) => {
                    eprintln!("Error updating {}: {}", repo_str, e);
                    false
                }
            }
        }
    });
    
    let results = futures::future::join_all(futs).await;
    let success_count = results.iter().filter(|&&success| success).count();
    println!("Refresh completed: {}/{} repos updated successfully", success_count, REPOS.len());
}

//────────────────── handlers
type Shared = Arc<Ctx>;

async fn networks(State(ctx): State<Shared>)
    -> Json<HashMap<&'static str,Option<VersionInfo>>> {
    Json(ctx.net.iter()
        .map(|(n,r)| (*n, ctx.map.get(r).map(|v| v.value().clone())))
        .collect())
}

async fn network(Path(net): Path<String>, State(ctx): State<Shared>)
    -> Json<Option<VersionInfo>> {
    Json(ctx.net.get(net.as_str())
        .and_then(|r| ctx.map.get(r).map(|v| v.value().clone())))
}

async fn repos(State(ctx): State<Shared>)
    -> Json<HashMap<&'static str,VersionInfo>> {
    Json(ctx.map.iter().map(|e|(*e.key(),e.value().clone())).collect())
}

async fn repo(Path(r): Path<String>, State(ctx): State<Shared>)
    -> Json<Option<VersionInfo>> {
    Json(ctx.map.get(r.as_str()).map(|v| v.value().clone()))
}

//────────────────── main
#[tokio::main]
async fn main() -> Result<()> {
    // Octocrab client (no timeout method in 0.44, omit)
    let gh = OctocrabBuilder::new()
        .add_header(USER_AGENT, "vermon/0.1".into())
        .add_header(ACCEPT, "application/vnd.github+json".into())
        .build()?;

    let ctx = Arc::new(Ctx {
        map: Arc::new(DashMap::new()),
        net: Arc::new(REPOS.iter()
            .flat_map(|(r,_,ns)| ns.iter().map(move |n| (*n,*r)))
            .collect()),
        gh,
    });

    println!("Starting initial refresh...");
    refresh(ctx.clone()).await;                         // initial fill

    // periodic refresh
    let bg = ctx.clone();
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(3600));
        loop { 
            tick.tick().await; 
            println!("Running scheduled refresh...");
            refresh(bg.clone()).await; 
        }
    });

    // routes
    let app = Router::new()
        .route("/api/networks",        get(networks))
        .route("/api/networks/{net}",  get(network))
        .route("/api/repos",           get(repos))
        .route("/api/repos/{repo}",    get(repo))
        .with_state(ctx);

    println!("Server starting on 0.0.0.0:3000");
    let addr: SocketAddr = "0.0.0.0:3000".parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
