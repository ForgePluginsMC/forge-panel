use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Port that must never be managed by the panel (reserved).
pub const FORBIDDEN_PORT: u16 = 25565;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PanelSettings {
    #[serde(default = "default_bind")]
    pub bind: String,
    pub data_dir: PathBuf,
    #[serde(default = "default_java")]
    pub default_java: String,
}

fn default_bind() -> String {
    "127.0.0.1:8090".to_string()
}

fn default_java() -> String {
    "java".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ServerRole {
    /// A regular game server (vanilla/paper/purpur/spigot...).
    #[default]
    Server,
    /// A proxy (Velocity). No EULA, gets a velocity.toml instead of server.properties.
    Proxy,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub name: String,
    pub dir: PathBuf,
    #[serde(default = "default_jar")]
    pub jar: String,
    #[serde(default)]
    pub java: Option<String>,
    #[serde(default)]
    pub jvm_args: Vec<String>,
    #[serde(default = "default_server_args")]
    pub server_args: Vec<String>,
    pub port: u16,
    #[serde(default)]
    pub rcon_port: Option<u16>,
    #[serde(default)]
    pub rcon_password: Option<String>,
    /// GS4 query port (needs enable-query=true in server.properties).
    #[serde(default)]
    pub query_port: Option<u16>,
    /// RAM limits in MB. Applied as -Xms/-Xmx at start (override jvm_args).
    #[serde(default)]
    pub xms_mb: Option<u32>,
    #[serde(default)]
    pub xmx_mb: Option<u32>,
    #[serde(default)]
    pub role: ServerRole,
    /// Name of the proxy this server sits behind (must reference a proxy entry).
    #[serde(default)]
    pub behind_proxy: Option<String>,
    /// Minecraft version of the server (e.g. "26.2"), used to filter plugin
    /// versions in the plugin browser. Optional; set from the UI.
    #[serde(default)]
    pub mc_version: Option<String>,
    /// If set, this entry is a REMOTE server: it runs on another machine and
    /// the panel only talks to it over RCON (host = this value). The panel
    /// never starts/stops it, binds no local ports for it, and offers no
    /// file/backup/plugin management — just status, players, and an RCON
    /// console. Local servers leave this unset and need no RCON at all.
    #[serde(default)]
    pub remote_host: Option<String>,
}

fn default_jar() -> String {
    "server.jar".to_string()
}

fn default_server_args() -> Vec<String> {
    vec!["--nogui".to_string()]
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Config {
    #[serde(default)]
    pub panel: Option<PanelSettings>,
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
}

impl ServerConfig {
    /// True when this entry is a remote server (managed over RCON only).
    pub fn is_remote(&self) -> bool {
        self.remote_host
            .as_deref()
            .map(|h| !h.trim().is_empty())
            .unwrap_or(false)
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Config = toml::from_str(&text).context("parsing config TOML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        let mut seen: HashMap<u16, String> = HashMap::new();
        let mut claim = |port: u16, what: &str, server: &str| -> Result<()> {
            if port == FORBIDDEN_PORT {
                bail!(
                    "refusing to manage server '{}': port {} is reserved/off-limits",
                    server, FORBIDDEN_PORT
                );
            }
            if let Some(other) = seen.insert(port, format!("{} ({})", server, what)) {
                bail!(
                    "port conflict: {} is claimed by both '{}' and server '{}'",
                    port, other, server
                );
            }
            Ok(())
        };
        for s in &self.servers {
            if s.name.trim().is_empty() {
                bail!("server entry with empty name");
            }
            if s.is_remote() {
                // Remote entries bind no local ports, so they claim nothing —
                // but the 25565 ban and the RCON requirement still apply.
                for p in [s.port].into_iter().chain(s.rcon_port).chain(s.query_port) {
                    if p == FORBIDDEN_PORT {
                        bail!(
                            "refusing remote server '{}': port {} is reserved/off-limits",
                            s.name, p
                        );
                    }
                }
                if s.rcon_port.is_none() {
                    bail!(
                        "remote server '{}' needs rcon_port (RCON is how the panel talks to it)",
                        s.name
                    );
                }
                continue;
            }
            claim(s.port, "game", &s.name)?;
            if let Some(rp) = s.rcon_port {
                claim(rp, "rcon", &s.name)?;
            }
            if let Some(qp) = s.query_port {
                claim(qp, "query", &s.name)?;
            }
        }
        // Proxy links must point at real proxies.
        for s in &self.servers {
            if let Some(proxy) = &s.behind_proxy {
                match self.find(proxy) {
                    Some(p) if p.role == ServerRole::Proxy => {}
                    Some(_) => bail!(
                        "server '{}' links behind_proxy='{}' which is not a proxy",
                        s.name, proxy
                    ),
                    None => bail!(
                        "server '{}' links behind_proxy='{}' which is not configured",
                        s.name, proxy
                    ),
                }
                if s.role == ServerRole::Proxy {
                    bail!("proxy '{}' cannot itself sit behind a proxy", s.name);
                }
            }
        }
        Ok(())
    }

    pub fn panel(&self) -> PanelSettings {
        self.panel.clone().unwrap_or(PanelSettings {
            bind: default_bind(),
            data_dir: PathBuf::from("./forge-panel-data"),
            default_java: default_java(),
        })
    }

    pub fn find(&self, name: &str) -> Option<&ServerConfig> {
        self.servers.iter().find(|s| s.name == name)
    }

    pub fn find_mut(&mut self, name: &str) -> Option<&mut ServerConfig> {
        self.servers.iter_mut().find(|s| s.name == name)
    }

    /// Ports already claimed by configured servers (game + rcon + query).
    pub fn used_ports(&self) -> Vec<u16> {
        let mut v = Vec::new();
        for s in &self.servers {
            v.push(s.port);
            if let Some(rp) = s.rcon_port {
                v.push(rp);
            }
            if let Some(qp) = s.query_port {
                v.push(qp);
            }
        }
        v
    }
}

/// Write the whole config back to disk (used after in-UI edits like RAM or
/// proxy links). Note: TOML comments in the file are not preserved.
pub fn write_config(path: &Path, cfg: &Config) -> Result<()> {
    cfg.validate()?;
    let text = toml::to_string_pretty(cfg).context("serializing config")?;
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Append a new [[servers]] entry to the config file without touching the rest.
pub fn append_server(path: &Path, server: &ServerConfig) -> Result<()> {
    let mut block = String::from("\n[[servers]]\n");
    block.push_str(&format!("name = {:?}\n", server.name));
    block.push_str(&format!("dir = {:?}\n", server.dir.to_string_lossy()));
    block.push_str(&format!("jar = {:?}\n", server.jar));
    if let Some(java) = &server.java {
        block.push_str(&format!("java = {:?}\n", java));
    }
    if !server.jvm_args.is_empty() {
        let args: Vec<String> = server.jvm_args.iter().map(|a| format!("{:?}", a)).collect();
        block.push_str(&format!("jvm_args = [{}]\n", args.join(", ")));
    }
    if !server.server_args.is_empty() {
        let args: Vec<String> = server.server_args.iter().map(|a| format!("{:?}", a)).collect();
        block.push_str(&format!("server_args = [{}]\n", args.join(", ")));
    }
    block.push_str(&format!("port = {}\n", server.port));
    if let Some(rp) = server.rcon_port {
        block.push_str(&format!("rcon_port = {}\n", rp));
    }
    if let Some(pw) = &server.rcon_password {
        block.push_str(&format!("rcon_password = {:?}\n", pw));
    }
    if let Some(qp) = server.query_port {
        block.push_str(&format!("query_port = {}\n", qp));
    }
    if let Some(xms) = server.xms_mb {
        block.push_str(&format!("xms_mb = {}\n", xms));
    }
    if let Some(xmx) = server.xmx_mb {
        block.push_str(&format!("xmx_mb = {}\n", xmx));
    }
    if server.role == ServerRole::Proxy {
        block.push_str("role = \"proxy\"\n");
    }
    if let Some(bp) = &server.behind_proxy {
        block.push_str(&format!("behind_proxy = {:?}\n", bp));
    }
    if let Some(mcv) = &server.mc_version {
        block.push_str(&format!("mc_version = {:?}\n", mcv));
    }
    if let Some(rh) = &server.remote_host {
        block.push_str(&format!("remote_host = {:?}\n", rh));
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let new_text = format!("{}{}", existing, block);
    // Validate the merged file before writing.
    let merged: Config = toml::from_str(&new_text).context("merged config invalid")?;
    merged.validate()?;
    std::fs::write(path, new_text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(name: &str, port: u16) -> ServerConfig {
        ServerConfig {
            name: name.to_string(),
            dir: PathBuf::from("/tmp"),
            jar: "server.jar".to_string(),
            java: None,
            jvm_args: vec![],
            server_args: vec![],
            port,
            rcon_port: None,
            rcon_password: None,
            query_port: None,
            xms_mb: None,
            xmx_mb: None,
            role: ServerRole::Server,
            behind_proxy: None,
            mc_version: None,
            remote_host: None,
        }
    }

    fn remote(name: &str, port: u16) -> ServerConfig {
        let mut s = local(name, port);
        s.remote_host = Some("192.168.1.50".to_string());
        s.rcon_port = Some(25575);
        s.rcon_password = Some("secret".to_string());
        s
    }

    #[test]
    fn remote_needs_rcon_port() {
        let mut s = remote("r1", 25570);
        s.rcon_port = None;
        let cfg = Config { panel: None, servers: vec![s] };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn remote_skips_local_port_claims() {
        // Same game port as a local server is fine: it's on another machine.
        let cfg = Config {
            panel: None,
            servers: vec![local("l1", 25570), remote("r1", 25570)],
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn remote_still_bans_25565() {
        let cfg = Config {
            panel: None,
            servers: vec![remote("r1", 25565)],
        };
        assert!(cfg.validate().is_err());
        let cfg = Config {
            panel: None,
            servers: vec![local("l1", 25570), {
                let mut r = remote("r2", 25571);
                r.rcon_port = Some(25565);
                r
            }],
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn local_port_conflict_still_caught() {
        let cfg = Config {
            panel: None,
            servers: vec![local("l1", 25570), local("l2", 25570)],
        };
        assert!(cfg.validate().is_err());
    }
}
