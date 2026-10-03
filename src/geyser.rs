use crate::config::{ServerConfig, ServerRole};
use crate::AppState;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::sync::Arc;

const GEYSER_UA: &str = "forge-panel/0.1.0 (+https://github.com/ForgePluginsMC)";
// Verified from the GeyserMC wiki (download.geysermc.org v2 API).
const GEYSER_URL: &str =
    "https://download.geysermc.org/v2/projects/geyser/versions/latest/builds/latest/downloads/spigot";
const FLOODGATE_URL: &str =
    "https://download.geysermc.org/v2/projects/floodgate/versions/latest/builds/latest/downloads/spigot";

#[derive(Debug, Clone, Serialize)]
pub struct GeyserStatus {
    pub installed: bool,
    pub floodgate: bool,
    pub bedrock_port: Option<u16>,
}

/// Geyser jar present? Floodgate jar present? Bedrock port from config?
pub fn status(cfg: &ServerConfig) -> GeyserStatus {
    let plugins = cfg.dir.join("plugins");
    let installed = plugins
        .read_dir()
        .map(|rd| {
            rd.filter_map(|e| e.ok()).any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .to_lowercase()
                    .starts_with("geyser")
                    && e.path().extension().map(|x| x == "jar").unwrap_or(false)
            })
        })
        .unwrap_or(false);
    let floodgate = plugins
        .read_dir()
        .map(|rd| {
            rd.filter_map(|e| e.ok()).any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .to_lowercase()
                    .starts_with("floodgate")
                    && e.path().extension().map(|x| x == "jar").unwrap_or(false)
            })
        })
        .unwrap_or(false);
    let bedrock_port = geyser_config_port(cfg);
    GeyserStatus {
        installed,
        floodgate,
        bedrock_port,
    }
}

/// Scan plugins/Geyser-Spigot/config.yml for the bedrock listener port.
fn geyser_config_port(cfg: &ServerConfig) -> Option<u16> {
    let path = cfg.dir.join("plugins").join("Geyser-Spigot").join("config.yml");
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_bedrock = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        // A new top-level key ends the bedrock section (no leading whitespace).
        if !line.starts_with(' ') && !line.starts_with('\t') && !trimmed.is_empty() {
            in_bedrock = trimmed.trim_end_matches(':') == "bedrock";
            continue;
        }
        if in_bedrock {
            if let Some(rest) = trimmed.strip_prefix("port:") {
                if let Ok(p) = rest.trim().parse::<u16>() {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// One-click Geyser + Floodgate install: downloads both jars into plugins/,
/// sets auth-type: floodgate when the Geyser config already exists, then
/// restarts the server so everything loads.
pub async fn install(state: Arc<AppState>, name: &str) -> Result<String> {
    let (cfg, all) = {
        let c = state.config.read().unwrap();
        let cfg = c
            .find(name)
            .cloned()
            .with_context(|| format!("unknown server '{}'", name))?;
        (cfg, c.servers.clone())
    };
    if cfg.role == ServerRole::Proxy {
        bail!("Geyser is a game-server plugin; it can't go on a proxy");
    }

    let plugins = cfg.dir.join("plugins");
    std::fs::create_dir_all(&plugins).context("creating plugins dir")?;

    let mut notes = Vec::new();
    for (url, file) in [
        (GEYSER_URL, "Geyser-Spigot.jar"),
        (FLOODGATE_URL, "floodgate-spigot.jar"),
    ] {
        let dest = plugins.join(file);
        if dest.exists() {
            notes.push(format!("{} already present, skipping download", file));
            continue;
        }
        let bytes = state
            .http
            .get(url)
            .header("User-Agent", GEYSER_UA)
            .send()
            .await
            .with_context(|| format!("downloading {}", file))?
            .error_for_status()
            .with_context(|| format!("{} download http error", file))?
            .bytes()
            .await
            .context("reading download body")?;
        if bytes.len() < 10_000 {
            bail!(
                "{} download looked wrong ({} bytes) — refusing to install",
                file,
                bytes.len()
            );
        }
        std::fs::write(&dest, &bytes).with_context(|| format!("writing {}", file))?;
        notes.push(format!("installed {} ({:.1} MB)", file, bytes.len() as f64 / 1_048_576.0));
    }

    // Floodgate auth-type: patch the Geyser config if it exists yet (it is
    // generated on first boot, so on a fresh install this happens next restart).
    let geyser_cfg = plugins.join("Geyser-Spigot").join("config.yml");
    if geyser_cfg.is_file() {
        let text = std::fs::read_to_string(&geyser_cfg).context("reading geyser config")?;
        let mut patched = false;
        let new_text: Vec<String> = text
            .lines()
            .map(|l| {
                let t = l.trim_start();
                if t.starts_with("auth-type:") && !patched {
                    patched = true;
                    format!("{}auth-type: floodgate", &l[..l.len() - t.len()])
                } else {
                    l.to_string()
                }
            })
            .collect();
        if patched {
            std::fs::write(&geyser_cfg, new_text.join("\n")).context("writing geyser config")?;
            notes.push("set auth-type: floodgate in Geyser config".to_string());
        }
    } else {
        notes.push(
            "Geyser config not generated yet — after the restart, set auth-type: floodgate in plugins/Geyser-Spigot/config.yml (or re-run this installer)".to_string(),
        );
    }

    // Restart so the plugins load.
    let java = cfg
        .java
        .clone()
        .unwrap_or_else(|| state.config.read().unwrap().panel().default_java);
    state.servers.restart(&cfg, &java, &all).await?;
    notes.push(format!("restarted '{}'", name));
    Ok(notes.join("; "))
}
