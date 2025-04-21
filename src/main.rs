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

//────────────────── static mapping
static REPOS: &[(&str, &str, &[&str])] = &[
    ("paritytech/polkadot-sdk","polkadot-stable",&[
        "polkadot","kusama","westend","paseo",
        "asset-hub-polkadot","bridge-hub-polkadot","coretime-polkadot","people-polkadot","collectives-polkadot",
        "asset-hub-kusama","bridge-hub-kusama","coretime-kusama","people-kusama",
        "asset-hub-westend","bridge-hub-westend","coretime-westend","people-westend","collectives-westend",
        "asset-hub-paseo","bridge-hub-paseo","coretime-paseo","people-paseo",
    ]),
    ("galacticcouncil/hydration-node","v",&["hydration"]),
    ("encointer/encointer-parachain","",&["encointer"]),
    ("AcalaNetwork/Acala","",&["acala"]),
    ("moonbeam-foundation/moonbeam","",&["moonbeam"]),
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
#[derive(Deserialize)]
struct Release { tag_name: String }

async fn latest(gh: &Octocrab, repo:&str, pfx:&str) -> Result<(Version,String)> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let Release{tag_name} = gh.get::<Release,_,_>(&url, None::<&()>).await?;
    let v = Version::parse(
        tag_name.trim_start_matches(pfx).trim_start_matches('v').replace('-', ".").as_str()
    ).context("semver parse")?;
    Ok((v, tag_name))
}

//────────────────── cache refresh
async fn refresh(ctx: Arc<Ctx>) {
    let futs = REPOS.iter().map(|(repo,pfx,_)| {
        let ctx = ctx.clone();
        async move {
            let gh = ctx.gh.clone();
            if let Ok((v,tag)) = latest(&gh, repo, pfx).await {
                ctx.map.insert(*repo, VersionInfo{version:v.to_string(), tag, ts:Utc::now()});
            }
        }
    });
    futures::future::join_all(futs).await;
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
        .add_header(ACCEPT,     "application/vnd.github+json".into())
        .build()?;

    let ctx = Arc::new(Ctx {
        map: Arc::new(DashMap::new()),
        net: Arc::new(REPOS.iter()
            .flat_map(|(r,_,ns)| ns.iter().map(move |n| (*n,*r)))
            .collect()),
        gh,
    });

    refresh(ctx.clone()).await;                         // initial fill

    // periodic refresh
    let bg = ctx.clone();
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(3600));
        loop { tick.tick().await; refresh(bg.clone()).await; }
    });

    // routes
    let app = Router::new()
        .route("/api/networks",        get(networks))
        .route("/api/networks/{net}",  get(network))
        .route("/api/repos",           get(repos))
        .route("/api/repos/{repo}",    get(repo))
        .with_state(ctx);

    let addr: SocketAddr = "0.0.0.0:3000".parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
