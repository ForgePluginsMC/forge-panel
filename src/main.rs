mod auth;
mod backup;
mod config;
mod db;
mod geyser;
mod icons;
mod installer;
mod mc;
mod player;
mod plugins;
mod rcon;
mod routes;
mod servers;

use crate::config::Config;
use crate::db::Db;
use crate::installer::JobLog;
use crate::mc::RconPool;
use crate::servers::ServerManager;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Instant;

pub struct AppState {
    pub config: RwLock<Config>,
    pub config_path: PathBuf,
    pub data_dir: PathBuf,
    pub db: Db,
    pub servers: ServerManager,
    pub jobs: JobLog,
    /// Upstream version lists: key -> (fetched_at, versions). Always expires.
    pub version_cache: RwLock<HashMap<String, (Instant, Vec<installer::ApiVersion>)>>,
    /// Generic short-TTL cache for plugin API JSON: key -> (fetched_at, body).
    pub api_cache: RwLock<HashMap<String, (Instant, String)>>,
    pub http: reqwest::Client,
    /// Persistent RCON connections (one per server, not one per command).
    pub rcon: Arc<RconPool>,
    /// Player info cache: name -> (fetched_at, (online, max, names, source)).
    /// 20s TTL on hits, 10s on misses — dashboard + players tab share it so
    /// RCON chatter stays out of server logs.
    pub player_cache: RwLock<HashMap<String, (Instant, Option<(u32, u32, Vec<String>, String)>)>>,
    /// TPS cache: name -> (fetched_at, tps_1m). 15s TTL.
    pub tps_cache: RwLock<HashMap<String, (Instant, Option<f64>)>>,
}

impl AppState {
    /// Get-or-fetch with a TTL. Never returns stale data past `ttl`.
    pub async fn cached_get(
        &self,
        key: &str,
        ttl: std::time::Duration,
        url: &str,
        user_agent: &str,
    ) -> Result<String> {
        {
            let cache = self.api_cache.read().unwrap();
            if let Some((at, body)) = cache.get(key) {
                if at.elapsed() < ttl {
                    return Ok(body.clone());
                }
            }
        }
        let body = self
            .http
            .get(url)
            .header("User-Agent", user_agent)
            .send()
            .await
            .with_context(|| format!("fetching {}", url))?
            .error_for_status()
            .with_context(|| format!("http error from {}", url))?
            .text()
            .await
            .context("reading response body")?;
        self.api_cache
            .write()
            .unwrap()
            .insert(key.to_string(), (Instant::now(), body.clone()));
        Ok(body)
    }
}

fn print_usage() {
    eprintln!("usage: forge-panel [--config PATH]");
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("forge_panel=info")),
        )
        .init();

    let mut config_path = PathBuf::from("forge-panel.toml");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => {
                config_path = PathBuf::from(
                    args.next()
                        .unwrap_or_else(|| {
                            print_usage();
                            std::process::exit(2);
                        }),
                );
            }
            "-h" | "--help" => {
                print_usage();
                return Ok(());
            }
            _ => {
                print_usage();
                std::process::exit(2);
            }
        }
    }

    // Config load refuses duplicate ports outright.
    let cfg = Config::load(&config_path)?;
    let panel = cfg.panel();
    std::fs::create_dir_all(&panel.data_dir)
        .with_context(|| format!("creating data dir {}", panel.data_dir.display()))?;

    let db = Db::open(&panel.data_dir.join("forge-panel.db"))?;
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building http client")?;
    let rcon = Arc::new(RconPool::new());

    let state = Arc::new(AppState {
        config: RwLock::new(cfg),
        config_path: config_path.clone(),
        data_dir: panel.data_dir.clone(),
        db,
        servers: ServerManager::new(panel.data_dir.clone(), rcon.clone()),
        jobs: JobLog::new(),
        version_cache: RwLock::new(HashMap::new()),
        api_cache: RwLock::new(HashMap::new()),
        http,
        rcon,
        player_cache: RwLock::new(HashMap::new()),
        tps_cache: RwLock::new(HashMap::new()),
    });

    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind(&panel.bind)
        .await
        .with_context(|| format!("binding {}", panel.bind))?;
    tracing::info!("forge-panel listening on {}", panel.bind);
    axum::serve(listener, app)
        .await
        .context("serving http")?;
    Ok(())
}
