use crate::config::ServerConfig;
use crate::AppState;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

pub const MODRINTH_UA: &str = "forge-panel/0.1.0 (+https://github.com/ForgePluginsMC)";
pub const SPIGET_UA: &str = "forge-panel/0.1.0";
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);

// ---------------------------------------------------------------------------
// Modrinth
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModrinthHit {
    pub project_id: String,
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub icon_url: String,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub follows: u64,
}

#[derive(Debug, Deserialize)]
struct ModrinthSearch {
    #[serde(default)]
    hits: Vec<ModrinthHit>,
    #[serde(default)]
    total_hits: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModrinthFile {
    pub url: String,
    pub filename: String,
    #[serde(default)]
    pub primary: bool,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModrinthVersion {
    pub id: String,
    pub name: String,
    pub version_number: String,
    #[serde(default)]
    pub version_type: String,
    #[serde(default)]
    pub game_versions: Vec<String>,
    #[serde(default)]
    pub loaders: Vec<String>,
    #[serde(default)]
    pub files: Vec<ModrinthFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModrinthProject {
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub followers: u64,
    #[serde(default)]
    pub icon_url: String,
    #[serde(default)]
    pub categories: Vec<String>,
}

fn facets_json(project_type: &str, category: Option<&str>) -> String {
    // Filter by project type. Sanitized to an allowlist.
    let pt = match project_type {
        "plugin" | "mod" | "modpack" => project_type,
        _ => "plugin",
    };
    match category {
        Some(c) => format!(
            r#"[["project_type:{}"],["categories:{}"]]"#,
            pt,
            c.replace('"', "")
        ),
        None => format!(r#"[["project_type:{}"]]"#, pt),
    }
}

/// Search Modrinth. `sort`: relevance | downloads | follows | newest | updated.
/// `category`: optional Modrinth category slug (e.g. "economy").
/// `project_type`: plugin | mod | modpack.
pub async fn modrinth_search(
    state: &Arc<AppState>,
    query: &str,
    sort: &str,
    category: Option<&str>,
    project_type: &str,
) -> Result<(Vec<ModrinthHit>, u64)> {
    let key = format!(
        "modrinth:search:{}:{}:{}:{}",
        project_type,
        sort,
        query,
        category.unwrap_or("-")
    );
    let url = "https://api.modrinth.com/v2/search";
    // Build the full URL manually so facet JSON is encoded exactly once.
    let full = format!(
        "{}?query={}&limit=25&index={}&facets={}",
        url,
        urlencode(query),
        urlencode(sort),
        urlencode(&facets_json(project_type, category))
    );
    let body = state
        .cached_get(&key, CACHE_TTL, &full, MODRINTH_UA)
        .await?;
    let resp: ModrinthSearch = serde_json::from_str(&body).context("parsing modrinth search")?;
    Ok((resp.hits, resp.total_hits))
}

/// Live Modrinth category tags, filtered by project type.
/// The raw API returns tags for all project types — we keep only the requested
/// type's categories to avoid the mess of resolution tags (128X), shader tags (PBR), etc.
pub async fn modrinth_categories(
    state: &Arc<AppState>,
    project_type: &str,
) -> Result<Vec<serde_json::Value>> {
    let pt = match project_type {
        "plugin" | "mod" | "modpack" => project_type,
        _ => "plugin",
    };
    let key = format!("modrinth:tag:category:{}", pt);
    let body = state
        .cached_get(
            &key,
            CACHE_TTL,
            "https://api.modrinth.com/v2/tag/category",
            MODRINTH_UA,
        )
        .await?;
    let all: Vec<serde_json::Value> =
        serde_json::from_str(&body).context("parsing modrinth categories")?;
    // Keep only the requested project type's categories with header "categories".
    // Deduplicate by name (API has duplicates).
    let mut seen = std::collections::HashSet::new();
    let filtered: Vec<serde_json::Value> = all
        .into_iter()
        .filter(|c| {
            let cpt = c.get("project_type").and_then(|v| v.as_str()).unwrap_or("");
            let header = c.get("header").and_then(|v| v.as_str()).unwrap_or("");
            let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("");
            cpt == pt && header == "categories" && seen.insert(name.to_string())
        })
        .collect();
    Ok(filtered)
}

pub async fn modrinth_project(state: &Arc<AppState>, id: &str) -> Result<ModrinthProject> {
    validate_id(id)?;
    let key = format!("modrinth:project:{}", id);
    let url = format!("https://api.modrinth.com/v2/project/{}", id);
    let body = state.cached_get(&key, CACHE_TTL, &url, MODRINTH_UA).await?;
    Ok(serde_json::from_str(&body).context("parsing modrinth project")?)
}

/// List versions, optionally filtered by loaders + game versions (live query).
pub async fn modrinth_versions(
    state: &Arc<AppState>,
    id: &str,
    loaders: &[&str],
    game_versions: &[&str],
) -> Result<Vec<ModrinthVersion>> {
    validate_id(id)?;
    let loaders_q = serde_json::to_string(loaders).unwrap();
    let games_q = serde_json::to_string(game_versions).unwrap();
    let key = format!("modrinth:versions:{}:{}:{}", id, loaders_q, games_q);
    let url = format!(
        "https://api.modrinth.com/v2/project/{}/version?loaders={}&game_versions={}",
        id,
        urlencode(&loaders_q),
        urlencode(&games_q)
    );
    let body = state.cached_get(&key, CACHE_TTL, &url, MODRINTH_UA).await?;
    Ok(serde_json::from_str(&body).context("parsing modrinth versions")?)
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("bad project id");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Spiget
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpigetResource {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub external: bool,
    #[serde(default, rename = "testedVersions")]
    pub tested_versions: Vec<String>,
    #[serde(default)]
    pub likes: u64,
    #[serde(default)]
    pub downloads: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpigetVersion {
    pub id: u64,
    #[serde(default)]
    pub uuid: String,
}

pub async fn spiget_search(state: &Arc<AppState>, query: &str) -> Result<Vec<SpigetResource>> {
    let key = format!("spiget:search:{}", query);
    let url = format!(
        "https://api.spiget.org/v2/search/resources/{}?field=name&size=25&sort=-downloads",
        urlencode(query)
    );
    let body = state.cached_get(&key, CACHE_TTL, &url, SPIGET_UA).await?;
    Ok(serde_json::from_str(&body).context("parsing spiget search")?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpigetCategory {
    pub id: u64,
    pub name: String,
}

/// Live Spiget resource categories (deduped by name).
pub async fn spiget_categories(state: &Arc<AppState>) -> Result<Vec<SpigetCategory>> {
    let key = "spiget:categories".to_string();
    let body = state
        .cached_get(&key, CACHE_TTL, "https://api.spiget.org/v2/categories", SPIGET_UA)
        .await?;
    let cats: Vec<SpigetCategory> = serde_json::from_str(&body).context("parsing spiget categories")?;
    let mut seen = std::collections::HashSet::new();
    Ok(cats
        .into_iter()
        .filter(|c| seen.insert(c.name.clone()))
        .collect())
}

/// Top resources in a Spiget category.
pub async fn spiget_category(state: &Arc<AppState>, category_id: u64) -> Result<Vec<SpigetResource>> {
    let key = format!("spiget:category:{}", category_id);
    let url = format!(
        "https://api.spiget.org/v2/resources?category={}&size=25&sort=-downloads",
        category_id
    );
    let body = state.cached_get(&key, CACHE_TTL, &url, SPIGET_UA).await?;
    Ok(serde_json::from_str(&body).context("parsing spiget category")?)
}

pub async fn spiget_trending(state: &Arc<AppState>) -> Result<Vec<SpigetResource>> {
    let key = "spiget:trending".to_string();
    let url = "https://api.spiget.org/v2/resources?size=25&sort=-downloads";
    let body = state.cached_get(&key, CACHE_TTL, url, SPIGET_UA).await?;
    Ok(serde_json::from_str(&body).context("parsing spiget trending")?)
}

pub async fn spiget_resource(state: &Arc<AppState>, id: u64) -> Result<SpigetResource> {
    let key = format!("spiget:resource:{}", id);
    let url = format!("https://api.spiget.org/v2/resources/{}", id);
    let body = state.cached_get(&key, CACHE_TTL, &url, SPIGET_UA).await?;
    Ok(serde_json::from_str(&body).context("parsing spiget resource")?)
}

pub async fn spiget_versions(state: &Arc<AppState>, id: u64) -> Result<Vec<SpigetVersion>> {
    // Short TTL: version lists change often.
    let key = format!("spiget:versions:{}", id);
    let url = format!("https://api.spiget.org/v2/resources/{}/versions?size=50", id);
    let body = state
        .cached_get(&key, Duration::from_secs(5 * 60), &url, SPIGET_UA)
        .await?;
    Ok(serde_json::from_str(&body).context("parsing spiget versions")?)
}

// ---------------------------------------------------------------------------
// Install / list / delete
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct InstalledPlugin {
    pub file: String,
    pub size_bytes: u64,
}

pub fn installed_plugins(cfg: &ServerConfig) -> Vec<InstalledPlugin> {
    let plugins = cfg.dir.join("plugins");
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&plugins) {
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.extension().map(|x| x == "jar").unwrap_or(false) {
                out.push(InstalledPlugin {
                    file: e.file_name().to_string_lossy().to_string(),
                    size_bytes: e.metadata().map(|m| m.len()).unwrap_or(0),
                });
            }
        }
    }
    out.sort_by(|a, b| a.file.cmp(&b.file));
    out
}

pub fn delete_plugin(cfg: &ServerConfig, file: &str) -> Result<()> {
    let name = sanitize_filename(file)?;
    let p = cfg.dir.join("plugins").join(&name);
    if !p.is_file() {
        bail!("plugin not found");
    }
    std::fs::remove_file(&p).context("deleting plugin jar")?;
    Ok(())
}

fn sanitize_filename(name: &str) -> Result<String> {
    let base = name.rsplit('/').next().unwrap_or(name);
    let base = base.rsplit('\\').next().unwrap_or(base);
    if base.is_empty() || base.contains("..") {
        bail!("bad file name");
    }
    if !base
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' || c == '+')
    {
        bail!("bad file name");
    }
    if !base.to_lowercase().ends_with(".jar") {
        bail!("not a jar file");
    }
    Ok(base.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginSource {
    Modrinth,
    Spiget,
}

impl PluginSource {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "modrinth" => Some(PluginSource::Modrinth),
            "spiget" => Some(PluginSource::Spiget),
            _ => None,
        }
    }
}

/// Download a plugin jar into the server's plugins/ folder.
/// For Modrinth: `project` is the slug/id, `version` is an optional version id.
/// For Spiget: `project` is the numeric resource id, `version` is an optional version id.
pub async fn install_plugin(
    state: &Arc<AppState>,
    cfg: &ServerConfig,
    source: PluginSource,
    project: &str,
    version: Option<&str>,
    mc_version: Option<&str>,
) -> Result<String> {
    let plugins = cfg.dir.join("plugins");
    std::fs::create_dir_all(&plugins).context("creating plugins dir")?;

    let (url, filename) = match source {
        PluginSource::Modrinth => {
            validate_id(project)?;
            let loaders = ["paper", "spigot", "purpur", "bukkit"];
            let games: Vec<&str> = mc_version.map(|v| vec![v]).unwrap_or_default();
            let versions = modrinth_versions(state, project, &loaders, &games).await?;
            if versions.is_empty() {
                bail!("no compatible versions found on Modrinth");
            }
            let v = match version {
                Some(vid) => versions
                    .iter()
                    .find(|v| v.id == vid)
                    .with_context(|| format!("version {} not found", vid))?,
                None => &versions[0],
            };
            let file = v
                .files
                .iter()
                .find(|f| f.primary)
                .or_else(|| v.files.first())
                .context("version has no files")?;
            (file.url.clone(), file.filename.clone())
        }
        PluginSource::Spiget => {
            let id: u64 = project.parse().context("spiget project must be a numeric id")?;
            let dl_url = match version {
                Some(vid) => {
                    let vid: u64 = vid.parse().context("bad spiget version id")?;
                    format!(
                        "https://api.spiget.org/v2/resources/{}/versions/{}/download",
                        id, vid
                    )
                }
                None => format!("https://api.spiget.org/v2/resources/{}/download", id),
            };
            // We resolve the real file after download (redirects); use a sane name.
            let resource = spiget_resource(state, id).await.ok();
            let fallback = resource
                .map(|r| format!("{}.jar", r.name.replace(' ', "-")))
                .unwrap_or_else(|| format!("spiget-{}.jar", id));
            (dl_url, fallback)
        }
    };

    let filename = sanitize_filename(&filename)?;
    let dest = plugins.join(&filename);
    if dest.exists() {
        bail!("{} is already installed", filename);
    }

    let resp = state
        .http
        .get(&url)
        .header("User-Agent", MODRINTH_UA)
        .send()
        .await
        .with_context(|| format!("downloading {}", filename))?;
    // Premium/paid SpigotMC resources can't come through the API: non-200
    // here means "buy it on SpigotMC instead".
    if !resp.status().is_success() {
        if source == PluginSource::Spiget {
            bail!(
                "Spiget refused the download (paid/premium resources aren't servable via the API) — purchase it on SpigotMC instead"
            );
        }
        bail!("download failed: http {}", resp.status());
    }
    let bytes = resp.bytes().await.context("reading plugin bytes")?;
    if bytes.len() < 1024 {
        bail!("download looked wrong ({} bytes) — refusing to install", bytes.len());
    }
    // For Spiget the redirect may land on a differently-named file; the
    // content is what matters.
    std::fs::write(&dest, &bytes).context("writing plugin jar")?;
    Ok(format!(
        "installed {} ({:.1} KB) — restart the server to load it",
        filename,
        bytes.len() as f64 / 1024.0
    ))
}
