use crate::auth::new_secret;
use crate::config::{self, ServerConfig};
use crate::AppState;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

/// How long upstream version lists are cached. Never a baked-in
/// list — the cache always expires and refreshes from the live APIs.
const VERSION_CACHE_TTL: Duration = Duration::from_secs(30 * 60);

const USER_AGENT: &str = "forge-panel/0.1.0 (+https://github.com/ForgePluginsMC)";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerKind {
    Vanilla,
    Paper,
    Purpur,
    Spigot,
    Velocity,
}

impl ServerKind {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "vanilla" => Some(ServerKind::Vanilla),
            "paper" => Some(ServerKind::Paper),
            "purpur" => Some(ServerKind::Purpur),
            "spigot" => Some(ServerKind::Spigot),
            "velocity" => Some(ServerKind::Velocity),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ServerKind::Vanilla => "vanilla",
            ServerKind::Paper => "paper",
            ServerKind::Purpur => "purpur",
            ServerKind::Spigot => "spigot",
            ServerKind::Velocity => "velocity",
        }
    }

    /// Is this a proxy rather than a game server?
    pub fn is_proxy(&self) -> bool {
        matches!(self, ServerKind::Velocity)
    }

    /// Future server types (Fabric/Forge/NeoForge/Mohist) plug in here.
    pub fn all() -> Vec<ServerKind> {
        vec![
            ServerKind::Vanilla,
            ServerKind::Paper,
            ServerKind::Purpur,
            ServerKind::Spigot,
            ServerKind::Velocity,
        ]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiVersion {
    pub id: String,
    #[serde(default)]
    pub label: String,
    /// True for snapshots / pre-releases / release candidates.
    #[serde(default)]
    pub snapshot: bool,
}

/// Parse "1.20.6" / "26.3-rc-3" into (numeric parts, is_release).
/// Release builds sort above their own pre-releases: 26.3 > 26.3-rc-3.
fn parse_mc_version(id: &str) -> (Vec<u64>, bool) {
    let (core, pre) = match id.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (id, None),
    };
    let nums = core
        .split('.')
        .map(|p| p.parse::<u64>().unwrap_or(0))
        .collect::<Vec<_>>();
    (nums, pre.is_none())
}

fn cmp_mc_version(a: &str, b: &str) -> std::cmp::Ordering {
    let (an, ar) = parse_mc_version(a);
    let (bn, br) = parse_mc_version(b);
    let len = an.len().max(bn.len());
    for i in 0..len {
        let x = an.get(i).copied().unwrap_or(0);
        let y = bn.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            std::cmp::Ordering::Equal => continue,
            ord => return ord,
        }
    }
    // Same numbers: release beats pre-release ("26.3" > "26.3-rc-3").
    ar.cmp(&br)
}

/// Latest first.
fn sort_versions_desc(versions: &mut [ApiVersion]) {
    versions.sort_by(|a, b| cmp_mc_version(&b.id, &a.id));
}

#[cfg(test)]
mod version_sort_tests {
    use super::*;

    fn ids(v: &[ApiVersion]) -> Vec<&str> {
        v.iter().map(|x| x.id.as_str()).collect()
    }

    #[test]
    fn sorts_latest_first_across_schemes() {
        let mut v: Vec<ApiVersion> = ["1.15.2", "26.3", "1.12.2", "26.2", "1.20.6", "26.3-rc-3", "1.15"]
            .iter()
            .map(|s| ApiVersion { id: s.to_string(), label: String::new(), snapshot: false })
            .collect();
        sort_versions_desc(&mut v);
        assert_eq!(
            ids(&v),
            vec!["26.3", "26.3-rc-3", "26.2", "1.20.6", "1.15.2", "1.15", "1.12.2"]
        );
    }

    #[test]
    fn release_beats_own_prerelease() {
        assert_eq!(cmp_mc_version("26.3", "26.3-rc-3"), std::cmp::Ordering::Greater);
        assert_eq!(cmp_mc_version("1.21", "1.21-pre1"), std::cmp::Ordering::Greater);
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Running,
    Done,
    Failed(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallJob {
    pub id: String,
    pub kind: ServerKind,
    pub version: String,
    pub name: String,
    pub status: JobStatus,
    pub log: Vec<String>,
    pub created_at: u64,
}

pub struct JobLog {
    jobs: RwLock<HashMap<String, InstallJob>>,
}

impl JobLog {
    pub fn new() -> Self {
        JobLog {
            jobs: RwLock::new(HashMap::new()),
        }
    }

    pub fn create(&self, kind: ServerKind, version: &str, name: &str) -> String {
        let id = format!("{:x}", rand_id());
        let job = InstallJob {
            id: id.clone(),
            kind,
            version: version.to_string(),
            name: name.to_string(),
            status: JobStatus::Running,
            log: vec![format!(
                "starting {} {} install as '{}'",
                kind.as_str(),
                version,
                name
            )],
            created_at: now_secs(),
        };
        self.jobs.write().unwrap().insert(id.clone(), job);
        id
    }

    pub fn push(&self, id: &str, line: impl Into<String>) {
        if let Some(job) = self.jobs.write().unwrap().get_mut(id) {
            job.log.push(line.into());
            if job.log.len() > 2000 {
                let drain = job.log.len() - 2000;
                job.log.drain(0..drain);
            }
        }
    }

    pub fn finish(&self, id: &str, status: JobStatus) {
        if let Some(job) = self.jobs.write().unwrap().get_mut(id) {
            job.status = status;
        }
    }

    pub fn get(&self, id: &str) -> Option<InstallJob> {
        self.jobs.read().unwrap().get(id).cloned()
    }

    pub fn list(&self) -> Vec<InstallJob> {
        let mut v: Vec<InstallJob> = self.jobs.read().unwrap().values().cloned().collect();
        v.sort_by_key(|j| std::cmp::Reverse(j.created_at));
        v
    }
}

fn rand_id() -> u64 {
    use std::fs::File;
    use std::io::Read;
    let mut buf = [0u8; 8];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .ok();
    u64::from_le_bytes(buf)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Only allow safe characters in values interpolated into upstream URLs.
fn validate_segment(s: &str) -> Result<()> {
    if s.is_empty() || s.len() > 64 {
        bail!("bad version segment");
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        bail!("bad version segment: {:?}", s);
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 48 {
        bail!("server name must be 1-48 chars");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("server name may only contain letters, numbers, - and _");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Upstream version listing (all live, cached briefly)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct MojangManifest {
    versions: Vec<MojangVersionEntry>,
}
#[derive(Deserialize)]
struct MojangVersionEntry {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    url: String,
}
#[derive(Deserialize)]
struct MojangVersionMeta {
    downloads: HashMap<String, MojangDownload>,
}
#[derive(Deserialize)]
struct MojangDownload {
    url: String,
    sha1: Option<String>,
}

#[derive(Deserialize)]
struct PaperProjects {
    versions: HashMap<String, Vec<String>>,
}
#[derive(Deserialize)]
struct PaperBuild {
    id: u64,
    downloads: HashMap<String, PaperDownload>,
}
#[derive(Deserialize)]
struct PaperDownload {
    url: String,
    checksums: HashMap<String, String>,
}

#[derive(Deserialize)]
struct PurpurVersions {
    versions: Vec<String>,
}
#[derive(Deserialize)]
struct PurpurBuilds {
    builds: PurpurBuildList,
}
#[derive(Deserialize)]
struct PurpurBuildList {
    latest: String,
}

pub async fn list_versions(
    state: &Arc<AppState>,
    kind: ServerKind,
) -> Result<Vec<ApiVersion>> {
    // Spigot has no version-list API; the UI takes a --rev string instead.
    if kind == ServerKind::Spigot {
        return Ok(Vec::new());
    }

    let key = kind.as_str().to_string();
    {
        let cache = state.version_cache.read().unwrap();
        if let Some((at, versions)) = cache.get(&key) {
            if at.elapsed() < VERSION_CACHE_TTL {
                return Ok(versions.clone());
            }
        }
    }

    let mut versions = match kind {
        ServerKind::Vanilla => vanilla_versions(state).await?,
        ServerKind::Paper => paper_versions(state).await?,
        ServerKind::Purpur => purpur_versions(state).await?,
        ServerKind::Velocity => velocity_versions(state).await?,
        ServerKind::Spigot => unreachable!(),
    };
    sort_versions_desc(&mut versions);

    state
        .version_cache
        .write()
        .unwrap()
        .insert(key, (Instant::now(), versions.clone()));
    Ok(versions)
}

async fn vanilla_versions(state: &Arc<AppState>) -> Result<Vec<ApiVersion>> {
    let manifest: MojangManifest = state
        .http
        .get("https://piston-meta.mojang.com/mc/game/version_manifest_v2.json")
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .context("fetching mojang version manifest")?
        .error_for_status()
        .context("mojang manifest http error")?
        .json()
        .await
        .context("parsing mojang manifest")?;
    Ok(manifest
        .versions
        .into_iter()
        .map(|v| ApiVersion {
            id: v.id,
            label: v.kind.clone(),
            snapshot: v.kind != "release",
        })
        .collect())
}

async fn paper_versions(state: &Arc<AppState>) -> Result<Vec<ApiVersion>> {
    fill_versions(state, "paper", "paper").await
}

async fn velocity_versions(state: &Arc<AppState>) -> Result<Vec<ApiVersion>> {
    fill_versions(state, "velocity", "velocity").await
}

/// Shared Fill v3 version listing (Paper and Velocity live on the same API).
async fn fill_versions(
    state: &Arc<AppState>,
    project: &str,
    label: &str,
) -> Result<Vec<ApiVersion>> {
    validate_segment(project)?;
    let projects: PaperProjects = state
        .http
        .get(&format!("https://fill.papermc.io/v3/projects/{}", project))
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .with_context(|| format!("fetching {} versions", label))?
        .error_for_status()
        .with_context(|| format!("{} versions http error", label))?
        .json()
        .await
        .with_context(|| format!("parsing {} versions", label))?;
    let mut out = Vec::new();
    for (group, versions) in &projects.versions {
        for v in versions {
            out.push(ApiVersion {
                id: v.clone(),
                label: format!("{} {}", label, group),
                // Fill lists pre-releases as "26.3-rc-3" style ids.
                snapshot: v.contains('-'),
            });
        }
    }
    Ok(out)
}

async fn purpur_versions(state: &Arc<AppState>) -> Result<Vec<ApiVersion>> {
    let resp: PurpurVersions = state
        .http
        .get("https://api.purpurmc.org/v2/purpur/")
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .context("fetching purpur versions")?
        .error_for_status()
        .context("purpur versions http error")?
        .json()
        .await
        .context("parsing purpur versions")?;
    Ok(resp
        .versions
        .into_iter()
        .map(|v| ApiVersion {
            id: v.clone(),
            label: format!("purpur {}", v),
            snapshot: v.contains('-'),
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Download URL resolution (all live)
// ---------------------------------------------------------------------------

/// Resolve (download_url, expected_sha256_or_sha1).
async fn resolve_download(
    state: &Arc<AppState>,
    kind: ServerKind,
    version: &str,
) -> Result<(String, Option<String>, Option<String>)> {
    validate_segment(version)?;
    match kind {
        ServerKind::Vanilla => {
            let manifest: MojangManifest = state
                .http
                .get("https://piston-meta.mojang.com/mc/game/version_manifest_v2.json")
                .header("User-Agent", USER_AGENT)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let entry = manifest
                .versions
                .iter()
                .find(|v| v.id == version)
                .with_context(|| format!("vanilla version not found: {}", version))?;
            let meta: MojangVersionMeta = state
                .http
                .get(&entry.url)
                .header("User-Agent", USER_AGENT)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let dl = meta
                .downloads
                .get("server")
                .context("no server download in version meta")?;
            Ok((dl.url.clone(), dl.sha1.clone(), None))
        }
        ServerKind::Paper | ServerKind::Velocity => {
            let project = if kind == ServerKind::Paper { "paper" } else { "velocity" };
            let url = format!(
                "https://fill.papermc.io/v3/projects/{}/versions/{}/builds",
                project, version
            );
            let builds: Vec<PaperBuild> = state
                .http
                .get(&url)
                .header("User-Agent", USER_AGENT)
                .send()
                .await
                .with_context(|| format!("fetching {} builds", project))?
                .error_for_status()
                .with_context(|| format!("{} builds http error", project))?
                .json()
                .await
                .with_context(|| format!("parsing {} builds", project))?;
            let latest = builds
                .into_iter()
                .next()
                .with_context(|| format!("no builds for this {} version", project))?;
            let dl = latest
                .downloads
                .get("server:default")
                .context("no server:default download in build")?;
            Ok((
                dl.url.clone(),
                dl.checksums.get("sha256").cloned(),
                Some(format!("{} build #{}", project, latest.id)),
            ))
        }
        ServerKind::Purpur => {
            let url = format!(
                "https://api.purpurmc.org/v2/purpur/{}/latest/download",
                version
            );
            // Verify the version exists first (cheap metadata call) and report
            // the latest build in the job log.
            let meta_url = format!("https://api.purpurmc.org/v2/purpur/{}", version);
            let meta: PurpurBuilds = state
                .http
                .get(&meta_url)
                .header("User-Agent", USER_AGENT)
                .send()
                .await
                .context("fetching purpur builds")?
                .error_for_status()
                .context("purpur version not found")?
                .json()
                .await
                .context("parsing purpur builds")?;
            Ok((url, None, Some(format!("purpur build {}", meta.builds.latest))))
        }
        ServerKind::Spigot => bail!("spigot is built with BuildTools, not downloaded"),
    }
}

// ---------------------------------------------------------------------------
// Install orchestration
// ---------------------------------------------------------------------------

pub struct InstallRequest {
    pub kind: ServerKind,
    pub version: String,
    pub name: String,
    pub eula_accepted: bool,
    pub xms_mb: Option<u32>,
    pub xmx_mb: Option<u32>,
    pub port: Option<u16>,
    pub jvm_args: Vec<String>,
    pub online_mode: Option<bool>,
    pub whitelist: Option<bool>,
    pub difficulty: Option<String>,
    pub gamemode: Option<String>,
}

pub async fn start_install(state: Arc<AppState>, req: InstallRequest) -> Result<String> {
    validate_name(&req.name)?;
    if !req.eula_accepted {
        bail!("you must accept the Minecraft EULA to install a server");
    }
    if req.kind != ServerKind::Spigot {
        validate_segment(&req.version)?;
    }
    {
        let cfg = state.config.read().unwrap();
        if cfg.find(&req.name).is_some() {
            bail!("a server named '{}' is already configured", req.name);
        }
    }

    let job_id = state.jobs.create(req.kind, &req.version, &req.name);
    // Persist to DB so it survives restarts.
    let _ = state.db.save_job(&job_id, &req.name, req.kind.as_str(), &req.version, "running");
    let state2 = state.clone();
    let job_id2 = job_id.clone();
    tokio::spawn(async move {
        let result = run_install(state2.clone(), &job_id2, req).await;
        match result {
            Ok(()) => {
                state2.jobs.push(&job_id2, "install complete");
                state2.jobs.finish(&job_id2, JobStatus::Done);
                if let Some(j) = state2.jobs.get(&job_id2) {
                    let _ = state2.db.save_job(&j.id, &j.name, j.kind.as_str(), &j.version, "done");
                }
            }
            Err(e) => {
                state2
                    .jobs
                    .push(&job_id2, format!("FAILED: {:#}", e));
                state2.jobs.finish(&job_id2, JobStatus::Failed(format!("{:#}", e)));
                if let Some(j) = state2.jobs.get(&job_id2) {
                    let _ = state2.db.save_job(&j.id, &j.name, j.kind.as_str(), &j.version, "failed");
                }
            }
        }
    });
    Ok(job_id)
}

async fn run_install(state: Arc<AppState>, job_id: &str, req: InstallRequest) -> Result<()> {
    let log = |m: String| state.jobs.push(job_id, m);

    // Allocate ports.
    let (port, rcon_port) = allocate_ports(&state, req.port)?;
    log(format!("assigned ports: game {} / rcon {}", port, rcon_port));

    let server_dir = state.data_dir.join("servers").join(&req.name);
    if server_dir.exists() {
        bail!("directory already exists: {}", server_dir.display());
    }
    std::fs::create_dir_all(&server_dir).context("creating server dir")?;

    // Fetch / build the jar.
    let jar_name = "server.jar".to_string();
    let jar_path = server_dir.join(&jar_name);
    if req.kind == ServerKind::Spigot {
        build_spigot(&state, job_id, &req.version, &jar_path).await?;
    } else {
        let (url, _checksum, build_label) = resolve_download(&state, req.kind, &req.version).await?;
        if let Some(label) = build_label {
            log(format!("resolved {}", label));
        }
        log(format!("downloading {}", url));
        download_to_file(&state, job_id, &url, &jar_path).await?;
    }
    log(format!("jar ready: {}", jar_path.display()));

    let rcon_password = new_secret(24)?;
    if req.kind.is_proxy() {
        // Velocity: velocity.toml instead of server.properties/EULA.
        let secret = new_secret(32)?;
        let toml = format!(
            "# Generated by forge-panel for proxy '{name}'\n\
             # Add backend servers under [servers], e.g. lobby = \"127.0.0.1:25566\"\n\
             config-version = \"2.7\"\n\
             bind = \"0.0.0.0:{port}\"\n\
             motd = \"{name} - forge-panel proxy\"\n\
             show-max-players = 100\n\
             online-mode = true\n\
             player-info-forwarding-mode = \"modern\"\n\
             forwarding-secret = \"{secret}\"\n\
             \n\
             [servers]\n\
             \n\
             try = []\n\
             \n\
             [advanced]\n\
             compression-threshold = 256\n\
             connection-timeout = 5000\n\
             read-timeout = 30000\n",
            name = req.name,
            port = port,
            secret = secret,
        );
        std::fs::write(server_dir.join("velocity.toml"), toml)
            .context("writing velocity.toml")?;
        std::fs::write(server_dir.join("forwarding.secret"), format!("{}\n", secret))
            .context("writing forwarding.secret")?;
        log("wrote velocity.toml (add your backend servers under [servers])".to_string());
        log("forwarding secret saved to forwarding.secret — copy it to each backend's paper config".to_string());
    } else {
        // EULA + server.properties.
        std::fs::write(server_dir.join("eula.txt"), "eula=true\n").context("writing eula.txt")?;
        let props = format!(
            "server-port={}\nenable-rcon=true\nrcon.port={}\nrcon.password={}\nonline-mode={}\nwhite-list={}\ndifficulty={}\ngamemode={}\nmotd={}\n",
            port, rcon_port, rcon_password,
            req.online_mode.unwrap_or(true),
            req.whitelist.unwrap_or(false),
            req.difficulty.as_deref().unwrap_or("normal"),
            req.gamemode.as_deref().unwrap_or("survival"),
            req.name
        );
        std::fs::write(server_dir.join("server.properties"), props)
            .context("writing server.properties")?;
        log("wrote eula.txt and server.properties".to_string());
    }

    // Register in the config file (append-only, then reload).
    let xms = req.xms_mb.unwrap_or(1024);
    let xmx = req.xmx_mb.unwrap_or(2048);
    let mut jvm = vec![format!("-Xms{}M", xms), format!("-Xmx{}M", xmx)];
    jvm.extend(req.jvm_args.clone());
    let entry = ServerConfig {
        name: req.name.clone(),
        dir: server_dir.clone(),
        jar: jar_name,
        java: None,
        jvm_args: jvm,
        server_args: if req.kind.is_proxy() {
            Vec::new() // velocity takes no --nogui
        } else {
            vec!["--nogui".to_string()]
        },
        port,
        rcon_port: if req.kind.is_proxy() { None } else { Some(rcon_port) },
        rcon_password: if req.kind.is_proxy() { None } else { Some(rcon_password) },
        query_port: None,
        xms_mb: Some(xms),
        xmx_mb: Some(xmx),
        role: if req.kind.is_proxy() {
            config::ServerRole::Proxy
        } else {
            config::ServerRole::Server
        },
        behind_proxy: None,
        remote_host: None,
    };
    config::append_server(&state.config_path, &entry)?;
    let fresh = config::Config::load(&state.config_path)?;
    *state.config.write().unwrap() = fresh;
    log(format!(
        "registered '{}' — start it from the dashboard",
        req.name
    ));
    Ok(())
}

/// True if nothing is currently bound to this TCP port on localhost.
fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn allocate_ports(state: &Arc<AppState>, requested: Option<u16>) -> Result<(u16, u16)> {
    let used = state.config.read().unwrap().used_ports();
    let mut port = requested.unwrap_or(25565);
    if requested.is_none() {
        loop {
            if used.contains(&port) || !port_is_free(port) {
                port += 1;
                continue;
            }
            break;
        }
    }
    let mut rcon = port + 1000;
    while used.contains(&rcon) || !port_is_free(rcon) {
        rcon += 1;
    }
    if port >= 60000 || rcon >= 65500 {
        bail!("ran out of ports to allocate");
    }
    Ok((port, rcon))
}

async fn download_to_file(
    state: &Arc<AppState>,
    job_id: &str,
    url: &str,
    dest: &PathBuf,
) -> Result<()> {
    let resp = state
        .http
        .get(url)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .context("starting download")?
        .error_for_status()
        .context("download http error")?;
    let total = resp.content_length().unwrap_or(0);
    let mut stream = resp.bytes_stream();
    let mut file = tokio::fs::File::create(dest)
        .await
        .context("creating destination file")?;
    let mut downloaded: u64 = 0;
    let mut last_logged: u64 = 0;
    use tokio_stream::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading download chunk")?;
        file.write_all(&chunk).await.context("writing file")?;
        downloaded += chunk.len() as u64;
        if downloaded - last_logged > 10 * 1024 * 1024 {
            last_logged = downloaded;
            if total > 0 {
                state.jobs.push(
                    job_id,
                    format!(
                        "downloaded {:.1} MB / {:.1} MB",
                        downloaded as f64 / 1_048_576.0,
                        total as f64 / 1_048_576.0
                    ),
                );
            } else {
                state.jobs.push(
                    job_id,
                    format!("downloaded {:.1} MB", downloaded as f64 / 1_048_576.0),
                );
            }
        }
    }
    file.flush().await.context("flushing file")?;
    Ok(())
}

async fn build_spigot(
    state: &Arc<AppState>,
    job_id: &str,
    rev: &str,
    jar_dest: &PathBuf,
) -> Result<()> {
    let log = |m: String| state.jobs.push(job_id, m);

    // Sanity: git + java must exist.
    for (bin, what) in [("git", "git"), ("java", "a JDK")] {
        let ok = tokio::process::Command::new(bin)
            .arg("--version")
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            bail!("BuildTools needs {} on PATH, but '{}' was not found", what, bin);
        }
    }

    let bt_dir = state.data_dir.join("buildtools");
    std::fs::create_dir_all(&bt_dir).context("creating buildtools dir")?;
    let bt_jar = bt_dir.join("BuildTools.jar");
    if !bt_jar.exists() {
        log("downloading BuildTools.jar".to_string());
        download_to_file(
            state,
            job_id,
            "https://hub.spigotmc.org/jenkins/job/BuildTools/lastSuccessfulBuild/artifact/target/BuildTools.jar",
            &bt_jar,
        )
        .await?;
    } else {
        log("BuildTools.jar already cached".to_string());
    }

    let work = bt_dir.join(format!("work-{}", job_id));
    std::fs::create_dir_all(&work).context("creating buildtools work dir")?;
    log(format!(
        "running BuildTools --rev {} (this takes several minutes)",
        rev
    ));

    let mut child = tokio::process::Command::new("java")
        .arg("-jar")
        .arg(&bt_jar)
        .arg("--rev")
        .arg(rev)
        .current_dir(&work)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawning BuildTools")?;

    // Stream both outputs into the job log.
    if let Some(out) = child.stdout.take() {
        pump_to_job_log(state.clone(), job_id.to_string(), out);
    }
    if let Some(err) = child.stderr.take() {
        pump_to_job_log(state.clone(), job_id.to_string(), err);
    }
    let status = child.wait().await.context("waiting for BuildTools")?;
    if !status.success() {
        bail!("BuildTools failed with status {}", status);
    }

    let built = work.join(format!("spigot-{}.jar", rev));
    if !built.exists() {
        bail!("BuildTools finished but {} was not produced", built.display());
    }
    tokio::fs::copy(&built, jar_dest)
        .await
        .context("copying spigot jar into server dir")?;
    log("spigot jar built and installed".to_string());
    Ok(())
}

fn pump_to_job_log(
    state: Arc<AppState>,
    job_id: String,
    pipe: impl tokio::io::AsyncRead + Unpin + Send + 'static,
) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    tokio::spawn(async move {
        let mut lines = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim().to_string();
            if !line.is_empty() {
                state.jobs.push(&job_id, line);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Import existing server directory
// ---------------------------------------------------------------------------

pub struct ImportRequest {
    pub name: String,
    pub dir: PathBuf,
    pub xms_mb: Option<u32>,
    pub xmx_mb: Option<u32>,
    pub port: Option<u16>,
    pub jvm_args: Vec<String>,
}

pub async fn import_server(state: Arc<AppState>, req: ImportRequest) -> Result<()> {
    validate_name(&req.name)?;
    if !req.dir.is_dir() {
        bail!("not a directory: {}", req.dir.display());
    }
    {
        let cfg = state.config.read().unwrap();
        if cfg.find(&req.name).is_some() {
            bail!("a server named '{}' is already configured", req.name);
        }
    }

    // Pick the jar: prefer common names, else first *.jar.
    let jar = ["server.jar", "paper.jar", "purpur.jar", "spigot.jar"]
        .iter()
        .map(|n| req.dir.join(n))
        .find(|p| p.is_file())
        .and_then(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            std::fs::read_dir(&req.dir)
                .ok()?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .find(|p| p.extension().map(|x| x == "jar").unwrap_or(false))
                .and_then(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_string())
                })
        })
        .context("no jar file found in that directory")?;

    // Read server-port from server.properties if present.
    let mut port: u16 = 0;
    let props_path = req.dir.join("server.properties");
    if props_path.is_file() {
        if let Ok(text) = std::fs::read_to_string(&props_path) {
            for line in text.lines() {
                let line = line.trim();
                if let Some(v) = line.strip_prefix("server-port=") {
                    port = v.trim().parse().unwrap_or(0);
                }
            }
        }
    }
    if port == 0 {
        // No server.properties yet: use requested port or allocate a fresh one.
        port = req.port.unwrap_or_else(|| allocate_ports(&state, None).map(|(p, _)| p).unwrap_or(25570));
    } else if let Some(p) = req.port {
        port = p;
    }
    let xms = req.xms_mb.unwrap_or(1024);
    let xmx = req.xmx_mb.unwrap_or(2048);
    let mut jvm = vec![format!("-Xms{}M", xms), format!("-Xmx{}M", xmx)];
    jvm.extend(req.jvm_args.clone());
    let entry = ServerConfig {
        name: req.name.clone(),
        dir: req.dir,
        jar,
        java: None,
        jvm_args: jvm,
        server_args: vec!["--nogui".to_string()],
        port,
        rcon_port: None,
        rcon_password: None,
        query_port: None,
        xms_mb: Some(xms),
        xmx_mb: Some(xmx),
        role: config::ServerRole::default(),
        behind_proxy: None,
        remote_host: None,
    };
    config::append_server(&state.config_path, &entry)?;
    let fresh = config::Config::load(&state.config_path)?;
    *state.config.write().unwrap() = fresh;
    Ok(())
}
