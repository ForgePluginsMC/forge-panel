use crate::rcon::{is_dead_connection, RconClient};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Mutex;

/// Persistent RCON connections, keyed by endpoint + password.
///
/// The old code connected fresh for every command, and each connect prints
/// "Thread RCON Client ... started / shutting down" into the server log —
/// with dashboard polling + TPS gauges that spammed the console constantly.
/// One pooled connection per server means one log line per server lifetime.
///
/// The map lock is never held across an `.await` (the `Arc` is cloned out
/// first) so pooled use stays `Send` for axum handlers.
///
/// Eviction is careful: only dead-connection errors drop the pooled client.
/// A command that fails for its own reasons (bad output, unknown command)
/// keeps the pool intact — otherwise one bad command would churn the
/// connection on every poll.
pub struct RconPool {
    clients: RwLock<HashMap<String, Arc<Mutex<RconClient>>>>,
}

impl RconPool {
    pub fn new() -> Self {
        RconPool {
            clients: RwLock::new(HashMap::new()),
        }
    }

    /// Run a command over a pooled connection. A dead pooled connection
    /// (server restart, idle timeout) is dropped and we reconnect once
    /// transparently.
    pub async fn run(
        &self,
        host: &str,
        port: u16,
        password: &str,
        command: &str,
    ) -> Result<String> {
        let key = format!("{}:{}:{}", host, port, password);
        let fut = async {
            let existing = self.clients.read().unwrap().get(&key).cloned();
            if let Some(slot) = existing {
                let mut client = slot.lock().await;
                match client.run_command(command).await {
                    Ok(out) => return Ok::<String, anyhow::Error>(out),
                    Err(e) if is_dead_connection(&e) => {
                        // Stale connection — drop it and reconnect below.
                        self.clients.write().unwrap().remove(&key);
                    }
                    Err(e) => {
                        // Command-level failure; the connection itself is fine.
                        return Err(e);
                    }
                }
            }
            let mut client = RconClient::connect(host, port, password).await?;
            let out = client.run_command(command).await?;
            self.clients
                .write()
                .unwrap()
                .insert(key, Arc::new(Mutex::new(client)));
            Ok(out)
        };
        tokio::time::timeout(Duration::from_secs(20), fut)
            .await
            .context("RCON timed out")?
    }
}

/// Parse `list` output like:
/// "There are 2 of a max of 20 players online: alice, bob"
/// "There are 0 of a max of 20 players online:"
pub fn parse_list_output(s: &str) -> (u32, u32, Vec<String>) {
    let s = s.trim();
    let rest = match s.strip_prefix("There are ") {
        Some(r) => r,
        None => return (0, 0, Vec::new()),
    };
    let mut parts = rest.splitn(2, " of a max of ");
    let online: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let rest = match parts.next() {
        Some(r) => r,
        None => return (online, 0, Vec::new()),
    };
    let mut parts = rest.splitn(2, " players online:");
    let max: u32 = parts.next().and_then(|p| p.trim().parse().ok()).unwrap_or(0);
    let names = parts
        .next()
        .map(|n| {
            n.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();
    (online, max, names)
}

/// Fallback player count via the Minecraft Query protocol (needs
/// enable-query=true in server.properties). Returns (online, max).
pub async fn query_players(host: &str, port: u16) -> Result<(u32, u32)> {
    // mc-query's stat_basic performs its own socket I/O; callers wrap this in a
    // timeout (players_quick) so the dashboard stays fast.
    let res = mc_query::query::stat_basic(host, port)
        .await
        .context("query protocol failed")?;
    Ok((res.num_players as u32, res.max_players as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_list_with_players() {
        let (o, m, names) =
            parse_list_output("There are 2 of a max of 20 players online: alice, bob");
        assert_eq!((o, m), (2, 20));
        assert_eq!(names, vec!["alice".to_string(), "bob".to_string()]);
    }

    #[test]
    fn parse_list_empty() {
        let (o, m, names) = parse_list_output("There are 0 of a max of 20 players online:");
        assert_eq!((o, m), (0, 20));
        assert!(names.is_empty());
    }

    #[test]
    fn parse_list_garbage() {
        let (o, m, names) = parse_list_output("something unexpected");
        assert_eq!((o, m), (0, 0));
        assert!(names.is_empty());
    }
}
