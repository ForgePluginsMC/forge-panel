use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
                // Remote entries bind no local ports, so they claim nothing.
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

/// Append a new server to the config file by reading, updating, and rewriting.
/// This avoids TOML duplicate-key errors from string concatenation.
pub fn append_server(path: &Path, server: &ServerConfig) -> Result<()> {
    let mut cfg = Config::load(path).unwrap_or_default();
    // Remove any existing entry with the same name to avoid duplicates.
    cfg.servers.retain(|s| s.name != server.name);
    cfg.servers.push(server.clone());
    write_config(path, &cfg)
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

    #[test]
    fn local_port_conflict_still_caught() {
        let cfg = Config {
            panel: None,
            servers: vec![local("l1", 25570), local("l2", 25570)],
        };
        assert!(cfg.validate().is_err());
    }
}
