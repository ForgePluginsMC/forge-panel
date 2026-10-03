use crate::auth::{self, SESSION_TTL_SECS};
use crate::backup;
use crate::config::{self, ServerConfig, ServerRole};
use crate::geyser;
use crate::installer::{self, InstallRequest, ServerKind};
use crate::mc;
use crate::plugins::{self, PluginSource};
use crate::servers::{self, ServerManager};
use crate::AppState;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse,
    },
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

type ApiResult<T> = Result<T, (StatusCode, Json<serde_json::Value>)>;

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({ "error": msg.into() })),
    )
}

fn server_or_404(state: &Arc<AppState>, name: &str) -> ApiResult<ServerConfig> {
    state
        .config
        .read()
        .unwrap()
        .find(name)
        .cloned()
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("unknown server '{}'", name)))
}

fn java_for(state: &Arc<AppState>, cfg: &ServerConfig) -> String {
    cfg.java
        .clone()
        .unwrap_or_else(|| state.config.read().unwrap().panel().default_java)
}

// ---------------------------------------------------------------------------
// Pages (embedded HTML)
// ---------------------------------------------------------------------------

async fn index_page() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}
async fn server_page() -> Html<&'static str> {
    Html(include_str!("../static/server.html"))
}
async fn install_page() -> Html<&'static str> {
    Html(include_str!("../static/install.html"))
}
async fn login_page() -> Html<&'static str> {
    Html(include_str!("../static/login.html"))
}

async fn style_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css")],
        include_str!("../static/style.css"),
    )
}
async fn app_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript")],
        include_str!("../static/app.js"),
    )
}

// ---------------------------------------------------------------------------
// Auth endpoints (public)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PasswordBody {
    password: String,
}

async fn setup_needed(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let needed = state.db.user_count().map(|n| n == 0).unwrap_or(true);
    Json(serde_json::json!({ "needed": needed }))
}

async fn setup(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PasswordBody>,
) -> ApiResult<impl IntoResponse> {
    if state.db.user_count().map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e)))? > 0 {
        return Err(err(StatusCode::FORBIDDEN, "already set up"));
    }
    if body.password.len() < 8 {
        return Err(err(StatusCode::BAD_REQUEST, "password must be at least 8 characters"));
    }
    let hash = tokio::task::spawn_blocking(move || auth::hash_password(&body.password))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e)))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e)))?;
    state
        .db
        .create_user("admin", &hash)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e)))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn login(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PasswordBody>,
) -> ApiResult<impl IntoResponse> {
    let hash = state
        .db
        .password_hash("admin")
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e)))?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "invalid credentials"))?;
    let pw = body.password.clone();
    let ok = tokio::task::spawn_blocking(move || auth::verify_password(&pw, &hash))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e)))?;
    if !ok {
        return Err(err(StatusCode::UNAUTHORIZED, "invalid credentials"));
    }
    let token = auth::new_token().map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e)))?;
    state
        .db
        .create_session(&token, SESSION_TTL_SECS)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e)))?;
    Ok((
        [(header::SET_COOKIE, auth::set_session_cookie(&token))],
        Json(serde_json::json!({ "ok": true })),
    ))
}

async fn logout(State(state): State<Arc<AppState>>, req: axum::extract::Request) -> impl IntoResponse {
    if let Some(t) = auth::cookie_token(&req) {
        state.db.delete_session(&t);
    }
    (
        [(header::SET_COOKIE, auth::clear_session_cookie())],
        Json(serde_json::json!({ "ok": true })),
    )
}

// ---------------------------------------------------------------------------
// Server status / control
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ServerInfo {
    name: String,
    port: u16,
    running: bool,
    rcon: bool,
    players_online: Option<u32>,
    players_max: Option<u32>,
    xms_mb: Option<u32>,
    xmx_mb: Option<u32>,
    memory_mb: Option<u64>,
    role: String,
    behind_proxy: Option<String>,
    mc_version: Option<String>,
    geyser: geyser::GeyserStatus,
    /// True for remote entries (RCON-only, managed on another machine).
    remote: bool,
    /// Remote host, when `remote` is true.
    host: Option<String>,
}

/// RCON/query target: the remote host for remote servers, localhost for
/// panel-managed ones.
fn rcon_host(cfg: &ServerConfig) -> &str {
    cfg.remote_host.as_deref().unwrap_or("127.0.0.1")
}

/// Guard for endpoints that need a local server directory / process.
fn reject_remote(cfg: &ServerConfig) -> ApiResult<()> {
    if cfg.is_remote() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "not available for remote servers (managed on their own host)",
        ));
    }
    Ok(())
}

/// Player info with a 20s hit / 10s miss TTL, shared by the dashboard and
/// the players tab. RCON first (host-aware, so remote servers work), then
/// the query protocol. Combined with the pooled RCON connections this keeps
/// "RCON Client started/shutting down" chatter out of server logs.
async fn cached_players(
    state: &Arc<AppState>,
    cfg: &ServerConfig,
) -> (Option<u32>, Option<u32>, Vec<String>, String) {
    {
        let cache = state.player_cache.read().unwrap();
        if let Some((at, val)) = cache.get(&cfg.name) {
            let ttl = Duration::from_secs(if val.is_some() { 20 } else { 10 });
            if at.elapsed() < ttl {
                return match val {
                    Some((o, m, names, src)) => {
                        (Some(*o), Some(*m), names.clone(), src.clone())
                    }
                    None => (None, None, Vec::new(), String::new()),
                };
            }
        }
    }
    let mut fetched: Option<(u32, u32, Vec<String>, String)> = None;
    let host = rcon_host(cfg);
    if let Some(rp) = cfg.rcon_port {
        let pw = cfg.rcon_password.as_deref().unwrap_or("");
        let fut = state.rcon.run(host, rp, pw, "list");
        if let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(4), fut).await {
            let (o, m, names) = mc::parse_list_output(&out);
            fetched = Some((o, m, names, "rcon".to_string()));
        }
    }
    if fetched.is_none() {
        let fut = mc::query_players(host, cfg.port);
        if let Ok(Ok((o, m))) = tokio::time::timeout(Duration::from_secs(4), fut).await {
            fetched = Some((o, m, Vec::new(), "query".to_string()));
        }
    }
    state
        .player_cache
        .write()
        .unwrap()
        .insert(cfg.name.clone(), (Instant::now(), fetched.clone()));
    match fetched {
        Some((o, m, names, src)) => (Some(o), Some(m), names, src),
        None => (None, None, Vec::new(), String::new()),
    }
}

/// Is the server up? Local = panel process state; remote = RCON reachable
/// (via the shared player cache, so this stays cheap).
async fn is_up(state: &Arc<AppState>, cfg: &ServerConfig) -> bool {
    if cfg.is_remote() {
        cached_players(state, cfg).await.0.is_some()
    } else {
        state.servers.is_running(cfg)
    }
}

async fn list_servers(State(state): State<Arc<AppState>>) -> Json<Vec<ServerInfo>> {
    let cfgs: Vec<ServerConfig> = state.config.read().unwrap().servers.clone();
    let mut out = Vec::new();
    for cfg in cfgs {
        let remote = cfg.is_remote();
        let running = is_up(&state, &cfg).await;
        let (po, pm) = if running {
            let (o, m, _, _) = cached_players(&state, &cfg).await;
            (o, m)
        } else {
            (None, None)
        };
        out.push(ServerInfo {
            name: cfg.name.clone(),
            port: cfg.port,
            running,
            rcon: cfg.rcon_port.is_some(),
            players_online: po,
            players_max: pm,
            xms_mb: cfg.xms_mb,
            xmx_mb: cfg.xmx_mb,
            memory_mb: if running && !remote {
                state.servers.memory_mb(&cfg)
            } else {
                None
            },
            role: match cfg.role {
                ServerRole::Proxy => "proxy".to_string(),
                ServerRole::Server => "server".to_string(),
            },
            behind_proxy: cfg.behind_proxy.clone(),
            mc_version: cfg.mc_version.clone(),
            geyser: if remote || cfg.role == ServerRole::Proxy {
                geyser::GeyserStatus {
                    installed: false,
                    floodgate: false,
                    bedrock_port: None,
                }
            } else {
                geyser::status(&cfg)
            },
            remote,
            host: cfg.remote_host.clone(),
        });
    }
    Json(out)
}

#[derive(Serialize)]
struct SystemInfo {
    ram_total_mb: Option<u64>,
    ram_allocated_mb: u64,
    ram_allocated_complete: bool,
}

async fn system_info(State(state): State<Arc<AppState>>) -> Json<SystemInfo> {
    let cfgs: Vec<ServerConfig> = state.config.read().unwrap().servers.clone();
    let (allocated, complete) = state.servers.allocated_mb(&cfgs);
    Json(SystemInfo {
        ram_total_mb: servers::system_ram_mb(),
        ram_allocated_mb: allocated,
        ram_allocated_complete: complete,
    })
}

#[derive(Serialize)]
struct TopologyProxy {
    name: String,
    port: u16,
    running: bool,
    backends: Vec<TopologyBackend>,
}
#[derive(Serialize)]
struct TopologyBackend {
    name: String,
    port: u16,
    running: bool,
}

async fn topology(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let cfgs: Vec<ServerConfig> = state.config.read().unwrap().servers.clone();
    let mut proxies = Vec::new();
    for p in cfgs.iter().filter(|s| s.role == ServerRole::Proxy) {
        let mut backends = Vec::new();
        for s in cfgs
            .iter()
            .filter(|s| s.behind_proxy.as_deref() == Some(p.name.as_str()))
        {
            backends.push(TopologyBackend {
                name: s.name.clone(),
                port: s.port,
                running: is_up(&state, s).await,
            });
        }
        proxies.push(TopologyProxy {
            name: p.name.clone(),
            port: p.port,
            running: is_up(&state, p).await,
            backends,
        });
    }
    let mut unlinked = Vec::new();
    for s in cfgs
        .iter()
        .filter(|s| s.role == ServerRole::Server && s.behind_proxy.is_none())
    {
        unlinked.push(serde_json::json!({
            "name": s.name,
            "port": s.port,
            "running": is_up(&state, s).await,
        }));
    }
    Json(serde_json::json!({ "proxies": proxies, "standalone": unlinked }))
}

async fn start_server(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let (cfg, all) = {
        let c = state.config.read().unwrap();
        let cfg = c
            .find(&name)
            .cloned()
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("unknown server '{}'", name)))?;
        (cfg, c.servers.clone())
    };
    reject_remote(&cfg)?;
    let java = java_for(&state, &cfg);
    state
        .servers
        .start(&cfg, &java, &all)
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{:#}", e)))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn stop_server(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = server_or_404(&state, &name)?;
    reject_remote(&cfg)?;
    state
        .servers
        .stop(&cfg)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e)))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn restart_server(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let (cfg, all) = {
        let c = state.config.read().unwrap();
        let cfg = c
            .find(&name)
            .cloned()
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("unknown server '{}'", name)))?;
        (cfg, c.servers.clone())
    };
    reject_remote(&cfg)?;
    let java = java_for(&state, &cfg);
    state
        .servers
        .restart(&cfg, &java, &all)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e)))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct SettingsBody {
    xms_mb: Option<u32>,
    xmx_mb: Option<u32>,
    mc_version: Option<String>,
    /// Proxy name to link behind, or empty string to unlink.
    behind_proxy: Option<String>,
}

/// Edit per-server settings (RAM, MC version, proxy link). Rewrites the
/// config file; TOML comments are not preserved.
async fn update_settings(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<SettingsBody>,
) -> ApiResult<Json<serde_json::Value>> {
    if let (Some(xms), Some(xmx)) = (body.xms_mb, body.xmx_mb) {
        if xms > xmx {
            return Err(err(StatusCode::BAD_REQUEST, "Xms cannot exceed Xmx"));
        }
    }
    for v in [&body.xms_mb, &body.xmx_mb].into_iter().flatten() {
        if *v == 0 || *v > 1_000_000 {
            return Err(err(StatusCode::BAD_REQUEST, "RAM must be 1..1000000 MB"));
        }
    }
    if let Some(mc) = &body.mc_version {
        if !mc.is_empty()
            && (mc.len() > 32
                || !mc
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_'))
        {
            return Err(err(StatusCode::BAD_REQUEST, "bad MC version"));
        }
    }
    let mut cfg = state.config.read().unwrap().clone();
    let entry = cfg
        .find_mut(&name)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("unknown server '{}'", name)))?;
    reject_remote(entry)?;
    entry.xms_mb = body.xms_mb;
    entry.xmx_mb = body.xmx_mb;
    entry.mc_version = body.mc_version.filter(|s| !s.is_empty());
    entry.behind_proxy = body.behind_proxy.filter(|s| !s.is_empty());
    config::write_config(&state.config_path, &cfg)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{:#}", e)))?;
    *state.config.write().unwrap() = cfg;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn install_geyser(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = server_or_404(&state, &name)?;
    reject_remote(&cfg)?;
    let message = geyser::install(state, &name)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e)))?;
    Ok(Json(serde_json::json!({ "ok": true, "message": message })))
}

#[derive(Deserialize)]
struct CommandBody {
    command: String,
}

async fn send_command(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<CommandBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = server_or_404(&state, &name)?;
    let cmd = body.command.trim();
    if cmd.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "empty command"));
    }
    if !cfg.is_remote() {
        // Local server: straight to the process stdin — no RCON needed.
        // (Falls through to RCON when the panel doesn't own the process,
        // e.g. the server was started outside the panel.)
        if state.servers.send_input(&cfg.name, cmd).await.is_ok() {
            return Ok(Json(serde_json::json!({ "ok": true, "via": "stdin" })));
        }
    }
    // Remote servers always go over RCON; local ones fall back to it.
    let port = cfg
        .rcon_port
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "RCON not configured for this server"))?;
    let pw = cfg.rcon_password.as_deref().unwrap_or("");
    let out = state
        .rcon
        .run(rcon_host(&cfg), port, pw, cmd)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("RCON failed: {:#}", e)))?;
    Ok(Json(serde_json::json!({ "output": out, "via": "rcon" })))
}

/// TPS (1m) via RCON `tps`, cached 15s. The instrument rack polls this
/// instead of the command endpoint so TPS never spams the console.
async fn tps(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = server_or_404(&state, &name)?;
    if cfg.role == ServerRole::Proxy {
        return Err(err(StatusCode::BAD_REQUEST, "proxies don't report TPS"));
    }
    {
        let cache = state.tps_cache.read().unwrap();
        if let Some((at, val)) = cache.get(&cfg.name) {
            if at.elapsed() < Duration::from_secs(15) {
                return Ok(Json(serde_json::json!({ "tps": val })));
            }
        }
    }
    let mut tps: Option<f64> = None;
    if let Some(rp) = cfg.rcon_port {
        let pw = cfg.rcon_password.as_deref().unwrap_or("");
        let fut = state.rcon.run(rcon_host(&cfg), rp, pw, "tps");
        if let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(4), fut).await {
            // Paper: "TPS from last 1m, 5m, 15m: 20.0, 20.0, 20.0"
            tps = out
                .split(':')
                .last()
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.trim().parse::<f64>().ok());
        }
    }
    state
        .tps_cache
        .write()
        .unwrap()
        .insert(cfg.name.clone(), (Instant::now(), tps));
    Ok(Json(serde_json::json!({ "tps": tps })))
}

#[derive(Serialize)]
struct PlayersInfo {
    online: Option<u32>,
    max: Option<u32>,
    names: Vec<String>,
    source: String,
}

async fn players(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<PlayersInfo>> {
    let cfg = server_or_404(&state, &name)?;
    let (o, m, names, src) = cached_players(&state, &cfg).await;
    match o {
        Some(online) => Ok(Json(PlayersInfo {
            online: Some(online),
            max: m,
            names,
            source: if src.is_empty() {
                "unknown".to_string()
            } else {
                src
            },
        })),
        None => Err(err(
            StatusCode::BAD_GATEWAY,
            "could not reach server (need RCON or enable-query)",
        )),
    }
}

// ---------------------------------------------------------------------------
// Console SSE
// ---------------------------------------------------------------------------

async fn console_stream(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>> {
    let cfg = server_or_404(&state, &name)?;
    if cfg.is_remote() {
        // No log file to tail on a remote host — the UI switches to an
        // RCON command/response terminal instead.
        return Err(err(
            StatusCode::BAD_REQUEST,
            "remote server has no local log; use RCON commands",
        ));
    }
    let path = ServerManager::tail_log_path(&cfg);
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(256);
    tokio::spawn(async move {
        tail_file(&path, tx).await;
    });
    let stream = ReceiverStream::new(rx);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new()))
}

async fn tail_file(path: &std::path::Path, tx: mpsc::Sender<Result<Event, Infallible>>) {
    async fn send_lines(text: &str, tx: &mpsc::Sender<Result<Event, Infallible>>) {
        for line in text.lines() {
            // Stop if the client disconnected.
            if tx.send(Ok(Event::default().data(line))).await.is_err() {
                break;
            }
        }
    }

    // Replay the tail first.
    let mut pos: u64 = 0;
    if let Ok(meta) = tokio::fs::metadata(path).await {
        pos = meta.len();
        let from = pos.saturating_sub(64 * 1024);
        if let Ok(mut f) = tokio::fs::File::open(path).await {
            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            if f.seek(std::io::SeekFrom::Start(from)).await.is_ok() {
                let mut buf = Vec::new();
                if f.read_to_end(&mut buf).await.is_ok() {
                    let text = String::from_utf8_lossy(&buf);
                    // Skip a possible partial first line.
                    let text = if from > 0 {
                        text.split_once('\n').map(|(_, rest)| rest).unwrap_or("")
                    } else {
                        &text
                    };
                    send_lines(text, &tx).await;
                }
            }
        }
    } else {
        let _ = tx
            .send(Ok(Event::default().data("(no log file yet — start the server)")))
            .await;
    }

    // Then poll for new data.
    loop {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let len = tokio::fs::metadata(path).await.map(|m| m.len()).unwrap_or(0);
        if len < pos {
            pos = 0; // rotated or truncated
        }
        if len > pos {
            if let Ok(mut f) = tokio::fs::File::open(path).await {
                use tokio::io::{AsyncReadExt, AsyncSeekExt};
                if f.seek(std::io::SeekFrom::Start(pos)).await.is_ok() {
                    let mut buf = vec![0u8; (len - pos) as usize];
                    if f.read_exact(&mut buf).await.is_ok() {
                        send_lines(&String::from_utf8_lossy(&buf), &tx).await;
                    }
                }
            }
            pos = len;
        }
        if tx.is_closed() {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// File browser
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PathQuery {
    path: Option<String>,
}

fn resolve_server_path(cfg: &ServerConfig, rel: &str) -> Result<std::path::PathBuf, String> {
    let base = cfg
        .dir
        .canonicalize()
        .map_err(|e| format!("server dir unreadable: {}", e))?;
    let rel = rel.trim_start_matches('/');
    if rel.split('/').any(|c| c == "..") {
        return Err("path escapes server directory".to_string());
    }
    let joined = base.join(rel);
    if let Ok(canon) = joined.canonicalize() {
        if !canon.starts_with(&base) {
            return Err("path escapes server directory".to_string());
        }
        Ok(canon)
    } else {
        // Doesn't exist yet (write path): lexical containment is enough here
        // since no symlinks can be involved in a non-existent path.
        if !joined.starts_with(&base) {
            return Err("path escapes server directory".to_string());
        }
        Ok(joined)
    }
}

#[derive(Serialize)]
struct FileEntry {
    name: String,
    is_dir: bool,
    size: u64,
    modified: i64,
}

async fn list_files(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<PathQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = server_or_404(&state, &name)?;
    reject_remote(&cfg)?;
    let rel = q.path.as_deref().unwrap_or("");
    let dir = resolve_server_path(&cfg, rel).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    if !dir.is_dir() {
        return Err(err(StatusCode::BAD_REQUEST, "not a directory"));
    }
    let mut entries = Vec::new();
    let rd = std::fs::read_dir(&dir).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
    for e in rd.filter_map(|e| e.ok()) {
        let md = e.metadata().ok();
        entries.push(FileEntry {
            name: e.file_name().to_string_lossy().to_string(),
            is_dir: md.as_ref().map(|m| m.is_dir()).unwrap_or(false),
            size: md.as_ref().map(|m| m.len()).unwrap_or(0),
            modified: md
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        });
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    Ok(Json(serde_json::json!({ "path": rel, "entries": entries })))
}

async fn read_file(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<PathQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = server_or_404(&state, &name)?;
    reject_remote(&cfg)?;
    let rel = q.path.as_deref().unwrap_or("");
    let p = resolve_server_path(&cfg, rel).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    let meta = std::fs::metadata(&p).map_err(|_| err(StatusCode::NOT_FOUND, "not found"))?;
    if meta.len() > 1024 * 1024 {
        return Err(err(StatusCode::BAD_REQUEST, "file too large to view (>1MB)"));
    }
    let bytes = std::fs::read(&p).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
    let content = String::from_utf8(bytes).map_err(|_| err(StatusCode::BAD_REQUEST, "not a text file"))?;
    Ok(Json(serde_json::json!({ "path": rel, "content": content })))
}

#[derive(Deserialize)]
struct WriteFileBody {
    path: String,
    content: String,
}

async fn write_file(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<WriteFileBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = server_or_404(&state, &name)?;
    reject_remote(&cfg)?;
    let p = resolve_server_path(&cfg, &body.path).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    if body.content.len() > 4 * 1024 * 1024 {
        return Err(err(StatusCode::BAD_REQUEST, "file too large (>4MB)"));
    }
    if let Some(parent) = p.parent() {
        if !parent.is_dir() {
            return Err(err(StatusCode::BAD_REQUEST, "parent directory does not exist"));
        }
    }
    std::fs::write(&p, body.content).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Backups
// ---------------------------------------------------------------------------

async fn list_backups_api(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = server_or_404(&state, &name)?;
    reject_remote(&cfg)?;
    let files = backup::list_backup_files(&state, &name);
    let out: Vec<serde_json::Value> = files
        .iter()
        .map(|f| {
            serde_json::json!({
                "name": f.name,
                "size_bytes": f.size_bytes,
                "created_at": f.created_at,
                "on_disk": f.on_disk,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "backups": out })))
}

async fn create_backup_api(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = server_or_404(&state, &name)?;
    reject_remote(&cfg)?;
    let file = backup::create_backup(state.clone(), &name)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e)))?;
    Ok(Json(serde_json::json!({ "ok": true, "file": file })))
}

async fn download_backup(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<impl IntoResponse> {
    let cfg = server_or_404(&state, &name)?;
    reject_remote(&cfg)?;
    let file = q.get("file").cloned().unwrap_or_default();
    let path = backup::backup_path(&state, &name, &file).map_err(|e| err(StatusCode::NOT_FOUND, format!("{:#}", e)))?;
    let bytes = std::fs::read(&path).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{}\"", file))
        .unwrap_or(HeaderValue::from_static("attachment"));
    Ok((
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("application/gzip")),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        Body::from(bytes),
    ))
}

async fn delete_backup_api(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = server_or_404(&state, &name)?;
    reject_remote(&cfg)?;
    let file = q.get("file").cloned().unwrap_or_default();
    backup::delete_backup(&state, &name, &file)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{:#}", e)))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Remote servers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RemoteBody {
    name: String,
    host: String,
    port: u16,
    rcon_port: u16,
    rcon_password: String,
    query_port: Option<u16>,
}

/// Register a remote server: runs on another machine, managed over RCON
/// only. No local process, no local ports, no files/backups/plugins.
async fn add_remote_server(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RemoteBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let name = body.name.trim();
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "name must be 1-64 chars: letters, numbers, - _",
        ));
    }
    let host = body.host.trim();
    if host.is_empty() || host.len() > 253 {
        return Err(err(StatusCode::BAD_REQUEST, "host is required"));
    }
    if body.rcon_password.len() > 256 {
        return Err(err(StatusCode::BAD_REQUEST, "rcon password too long"));
    }
    {
        let c = state.config.read().unwrap();
        if c.find(name).is_some() {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("a server named '{}' already exists", name),
            ));
        }
    }
    let entry = ServerConfig {
        name: name.to_string(),
        dir: PathBuf::from(""),
        jar: "server.jar".to_string(),
        java: None,
        jvm_args: Vec::new(),
        server_args: Vec::new(),
        port: body.port,
        rcon_port: Some(body.rcon_port),
        rcon_password: Some(body.rcon_password),
        query_port: body.query_port,
        xms_mb: None,
        xmx_mb: None,
        role: ServerRole::Server,
        behind_proxy: None,
        mc_version: None,
        remote_host: Some(host.to_string()),
    };
    // append_server validates the merged config (25565 ban, rcon required).
    config::append_server(&state.config_path, &entry)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{:#}", e)))?;
    let reloaded = config::Config::load(&state.config_path)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e)))?;
    *state.config.write().unwrap() = reloaded;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Installer API
// ---------------------------------------------------------------------------

async fn installer_types() -> Json<serde_json::Value> {
    let types: Vec<serde_json::Value> = ServerKind::all()
        .iter()
        .map(|k| {
            serde_json::json!({
                "id": k.as_str(),
                "needs_version_input": *k == ServerKind::Spigot,
                "version_hint": if *k == ServerKind::Spigot { "BuildTools --rev, e.g. 1.21" } else { "" },
            })
        })
        .collect();
    Json(serde_json::json!({ "types": types }))
}

async fn installer_versions(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let kind = q
        .get("type")
        .and_then(|t| ServerKind::from_str(t))
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "unknown server type"))?;
    let include_snapshots = q.get("snapshots").map(|v| v == "true").unwrap_or(false);
    let versions = installer::list_versions(&state, kind)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("upstream API failed: {:#}", e)))?;
    let versions: Vec<_> = if include_snapshots {
        versions
    } else {
        versions.into_iter().filter(|v| !v.snapshot).collect()
    };
    Ok(Json(serde_json::json!({ "versions": versions })))
}

#[derive(Deserialize)]
struct InstallBody {
    #[serde(rename = "type")]
    kind: String,
    version: String,
    name: String,
    eula: bool,
}

async fn installer_install(
    State(state): State<Arc<AppState>>,
    Json(body): Json<InstallBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let kind = ServerKind::from_str(&body.kind)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "unknown server type"))?;
    let req = InstallRequest {
        kind,
        version: body.version,
        name: body.name,
        eula_accepted: body.eula,
    };
    let job_id = installer::start_install(state, req)
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{:#}", e)))?;
    Ok(Json(serde_json::json!({ "job_id": job_id })))
}

async fn installer_jobs(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "jobs": state.jobs.list() }))
}

async fn installer_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let job = state
        .jobs
        .get(&id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown job"))?;
    Ok(Json(serde_json::to_value(job).unwrap()))
}

async fn installer_job_stream(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>> {
    if state.jobs.get(&id).is_none() {
        return Err(err(StatusCode::NOT_FOUND, "unknown job"));
    }
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(256);
    let state2 = state.clone();
    tokio::spawn(async move {
        let mut sent = 0usize;
        loop {
            let snapshot = state2.jobs.get(&id);
            let done = match &snapshot {
                Some(j) => {
                    for line in j.log.iter().skip(sent) {
                        let payload = serde_json::json!({
                            "line": line,
                            "status": j.status,
                        });
                        if tx.send(Ok(Event::default().data(payload.to_string()))).await.is_err() {
                            return;
                        }
                    }
                    sent = j.log.len();
                    !matches!(j.status, installer::JobStatus::Running)
                }
                None => true,
            };
            if done {
                // One last event so the UI knows we're finished.
                let _ = tx
                    .send(Ok(Event::default().event("done").data("done")))
                    .await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
    let stream = ReceiverStream::new(rx);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new()))
}

#[derive(Deserialize)]
struct ImportBody {
    name: String,
    dir: String,
}

async fn installer_import(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ImportBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let req = installer::ImportRequest {
        name: body.name,
        dir: std::path::PathBuf::from(body.dir),
    };
    installer::import_server(state, req)
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{:#}", e)))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Plugins (per-server tab)
// ---------------------------------------------------------------------------

async fn plugins_installed(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = server_or_404(&state, &name)?;
    reject_remote(&cfg)?;
    Ok(Json(
        serde_json::json!({ "plugins": plugins::installed_plugins(&cfg) }),
    ))
}

async fn plugins_delete(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = server_or_404(&state, &name)?;
    reject_remote(&cfg)?;
    let file = q.get("file").cloned().unwrap_or_default();
    plugins::delete_plugin(&cfg, &file)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{:#}", e)))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct PluginSearchQuery {
    q: Option<String>,
    sort: Option<String>,
    category: Option<String>,
}

async fn plugins_modrinth_search(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<PluginSearchQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    server_or_404(&state, &name)?;
    let sort = q.sort.as_deref().unwrap_or("relevance");
    let (hits, total) = plugins::modrinth_search(
        &state,
        q.q.as_deref().unwrap_or(""),
        sort,
        q.category.as_deref(),
    )
    .await
    .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("Modrinth failed: {:#}", e)))?;
    Ok(Json(serde_json::json!({ "hits": hits, "total": total })))
}

async fn plugins_modrinth_categories(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    server_or_404(&state, &name)?;
    let cats = plugins::modrinth_categories(&state)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("Modrinth failed: {:#}", e)))?;
    Ok(Json(serde_json::json!({ "categories": cats })))
}

async fn plugins_spiget_categories(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    server_or_404(&state, &name)?;
    let cats = plugins::spiget_categories(&state)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("Spiget failed: {:#}", e)))?;
    Ok(Json(serde_json::json!({ "categories": cats })))
}

async fn plugins_spiget_category(
    State(state): State<Arc<AppState>>,
    Path((name, id)): Path<(String, u64)>,
) -> ApiResult<Json<serde_json::Value>> {
    server_or_404(&state, &name)?;
    let hits = plugins::spiget_category(&state, id)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("Spiget failed: {:#}", e)))?;
    Ok(Json(serde_json::json!({ "hits": hits })))
}

async fn plugins_modrinth_trending(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    server_or_404(&state, &name)?;
    let (hits, total) = plugins::modrinth_search(&state, "", "downloads", None)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("Modrinth failed: {:#}", e)))?;
    Ok(Json(serde_json::json!({ "hits": hits, "total": total })))
}

async fn plugins_modrinth_project(
    State(state): State<Arc<AppState>>,
    Path((name, id)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    server_or_404(&state, &name)?;
    let project = plugins::modrinth_project(&state, &id)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("Modrinth failed: {:#}", e)))?;
    Ok(Json(serde_json::to_value(project).unwrap()))
}

async fn plugins_modrinth_versions(
    State(state): State<Arc<AppState>>,
    Path((name, id)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = server_or_404(&state, &name)?;
    let loaders = ["paper", "spigot", "purpur", "bukkit"];
    let games: Vec<&str> = cfg.mc_version.as_deref().map(|v| vec![v]).unwrap_or_default();
    let versions = plugins::modrinth_versions(&state, &id, &loaders, &games)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("Modrinth failed: {:#}", e)))?;
    Ok(Json(serde_json::json!({ "versions": versions })))
}

async fn plugins_spiget_search(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<PluginSearchQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    server_or_404(&state, &name)?;
    let hits = plugins::spiget_search(&state, q.q.as_deref().unwrap_or(""))
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("Spiget failed: {:#}", e)))?;
    Ok(Json(serde_json::json!({ "hits": hits })))
}

async fn plugins_spiget_trending(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    server_or_404(&state, &name)?;
    let hits = plugins::spiget_trending(&state)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("Spiget failed: {:#}", e)))?;
    Ok(Json(serde_json::json!({ "hits": hits })))
}

async fn plugins_spiget_resource(
    State(state): State<Arc<AppState>>,
    Path((name, id)): Path<(String, u64)>,
) -> ApiResult<Json<serde_json::Value>> {
    server_or_404(&state, &name)?;
    let resource = plugins::spiget_resource(&state, id)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("Spiget failed: {:#}", e)))?;
    Ok(Json(serde_json::to_value(resource).unwrap()))
}

async fn plugins_spiget_versions(
    State(state): State<Arc<AppState>>,
    Path((name, id)): Path<(String, u64)>,
) -> ApiResult<Json<serde_json::Value>> {
    server_or_404(&state, &name)?;
    let versions = plugins::spiget_versions(&state, id)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("Spiget failed: {:#}", e)))?;
    Ok(Json(serde_json::json!({ "versions": versions })))
}

#[derive(Deserialize)]
struct PluginInstallBody {
    source: String,
    project: String,
    version: Option<String>,
}

async fn plugins_install(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<PluginInstallBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = server_or_404(&state, &name)?;
    reject_remote(&cfg)?;
    if cfg.role == ServerRole::Proxy {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "plugins can't be installed on a proxy",
        ));
    }
    let source = PluginSource::from_str(&body.source)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "unknown source"))?;
    let mc_version = cfg.mc_version.clone();
    let message = plugins::install_plugin(
        &state,
        &cfg,
        source,
        &body.project,
        body.version.as_deref(),
        mc_version.as_deref(),
    )
    .await
    .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("{:#}", e)))?;
    Ok(Json(serde_json::json!({ "ok": true, "message": message })))
}

pub fn router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/", get(index_page))
        .route("/server/{name}", get(server_page))
        .route("/install", get(install_page))
        .route("/api/logout", post(logout))
        .route("/api/servers", get(list_servers))
        .route("/api/servers/remote", post(add_remote_server))
        .route("/api/servers/{name}/start", post(start_server))
        .route("/api/servers/{name}/stop", post(stop_server))
        .route("/api/servers/{name}/restart", post(restart_server))
        .route("/api/servers/{name}/command", post(send_command))
        .route("/api/servers/{name}/tps", get(tps))
        .route("/api/servers/{name}/players", get(players))
        .route("/api/servers/{name}/console", get(console_stream))
        .route("/api/servers/{name}/files", get(list_files))
        .route("/api/servers/{name}/file", get(read_file).post(write_file))
        .route("/api/servers/{name}/settings", post(update_settings))
        .route("/api/servers/{name}/geyser", post(install_geyser))
        .route("/api/system", get(system_info))
        .route("/api/topology", get(topology))
        .route("/api/servers/{name}/plugins", get(plugins_installed))
        .route("/api/servers/{name}/plugins/delete", delete(plugins_delete))
        .route("/api/servers/{name}/plugins/install", post(plugins_install))
        .route("/api/servers/{name}/plugins/modrinth/search", get(plugins_modrinth_search))
        .route("/api/servers/{name}/plugins/modrinth/categories", get(plugins_modrinth_categories))
        .route("/api/servers/{name}/plugins/modrinth/trending", get(plugins_modrinth_trending))
        .route("/api/servers/{name}/plugins/modrinth/project/{id}", get(plugins_modrinth_project))
        .route("/api/servers/{name}/plugins/modrinth/versions/{id}", get(plugins_modrinth_versions))
        .route("/api/servers/{name}/plugins/spiget/search", get(plugins_spiget_search))
        .route("/api/servers/{name}/plugins/spiget/categories", get(plugins_spiget_categories))
        .route("/api/servers/{name}/plugins/spiget/category/{id}", get(plugins_spiget_category))
        .route("/api/servers/{name}/plugins/spiget/trending", get(plugins_spiget_trending))
        .route("/api/servers/{name}/plugins/spiget/resource/{id}", get(plugins_spiget_resource))
        .route("/api/servers/{name}/plugins/spiget/versions/{id}", get(plugins_spiget_versions))
        .route("/api/backups/{name}", get(list_backups_api).post(create_backup_api))
        .route("/api/backups/{name}/download", get(download_backup))
        .route("/api/backups/{name}/delete", delete(delete_backup_api))
        .route("/api/installer/types", get(installer_types))
        .route("/api/installer/versions", get(installer_versions))
        .route("/api/installer/install", post(installer_install))
        .route("/api/installer/import", post(installer_import))
        .route("/api/installer/jobs", get(installer_jobs))
        .route("/api/installer/jobs/{id}", get(installer_job))
        .route("/api/installer/jobs/{id}/stream", get(installer_job_stream))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));

    let public = Router::new()
        .route("/login", get(login_page))
        .route("/static/style.css", get(style_css))
        .route("/static/app.js", get(app_js))
        .route("/api/setup-needed", get(setup_needed))
        .route("/api/setup", post(setup))
        .route("/api/login", post(login));

    public.merge(protected).with_state(state)
}
