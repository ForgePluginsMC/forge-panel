//! Minimal RCON client (wiki.vg protocol), written for the panel.
//!
//! We used to use `mc-query`'s RCON client, but it rejects any response with
//! non-ASCII bytes — and Paper's `tps` output contains `\u{a7}` color codes
//! (as UTF-8), so TPS was unobtainable and every attempt poisoned the
//! connection pool. This client treats payloads as UTF-8 (lossy), strips
//! Minecraft color codes, and frames multi-packet responses with the
//! mirror-packet trick instead of a payload-length heuristic.

use anyhow::{bail, Context, Result};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TYPE_LOGIN: i32 = 3;
const TYPE_COMMAND: i32 = 2;

/// Request ids: the command goes out as CMD_ID, then an empty "mirror"
/// packet as MIRROR_ID. Responses arrive in order over TCP, so the mirror
/// response marks the end of the command's (possibly multi-packet) output.
const CMD_ID: i32 = 10;
const MIRROR_ID: i32 = 11;

const READ_TIMEOUT: Duration = Duration::from_secs(10);

pub struct RconClient {
    stream: TcpStream,
}

struct Packet {
    request_id: i32,
    payload: Vec<u8>,
}

impl RconClient {
    pub async fn connect(host: &str, port: u16, password: &str) -> Result<Self> {
        let stream = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect((host, port)))
            .await
            .context("RCON connect timed out")?
            .context("connecting to RCON")?;
        let mut client = RconClient { stream };
        client.send_packet(1, TYPE_LOGIN, password.as_bytes()).await?;
        let resp = client.read_packet().await?;
        if resp.request_id == -1 {
            bail!("RCON authentication failed");
        }
        if resp.request_id != 1 {
            bail!("RCON auth: request id mismatch");
        }
        Ok(client)
    }

    /// Run a command and return its full output (multi-packet safe).
    /// Network failures surface as `std::io::Error` (so the pool can tell a
    /// dead connection from a command that merely failed).
    pub async fn run_command(&mut self, command: &str) -> Result<String> {
        self.send_packet(CMD_ID, TYPE_COMMAND, command.as_bytes())
            .await?;
        self.send_packet(MIRROR_ID, TYPE_COMMAND, b"").await?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let resp = self.read_packet().await?;
            if resp.request_id == MIRROR_ID {
                break;
            } else if resp.request_id == CMD_ID {
                out.extend_from_slice(&resp.payload);
            } else if resp.request_id == -1 {
                bail!("RCON: server reports not authenticated");
            }
            // Anything else is ignorable (stray/duplicate packets).
        }
        Ok(strip_minecraft_colors(&String::from_utf8_lossy(&out)))
    }

    async fn send_packet(&mut self, request_id: i32, ptype: i32, payload: &[u8]) -> Result<()> {
        let len = (4 + 4 + payload.len() + 2) as i32;
        let mut buf = Vec::with_capacity((len + 4) as usize);
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(&request_id.to_le_bytes());
        buf.extend_from_slice(&ptype.to_le_bytes());
        buf.extend_from_slice(payload);
        buf.push(0);
        buf.push(0);
        tokio::time::timeout(READ_TIMEOUT, self.stream.write_all(&buf))
            .await
            .context("RCON write timed out")?
            .context("writing RCON packet")?;
        Ok(())
    }

    async fn read_packet(&mut self) -> Result<Packet> {
        let len = tokio::time::timeout(READ_TIMEOUT, self.stream.read_i32_le())
            .await
            .context("RCON read timed out")?
            .context("reading RCON packet length")?;
        if len < 10 || len > 4096 * 16 {
            bail!("RCON: bogus packet length {}", len);
        }
        let mut buf = vec![0u8; len as usize];
        tokio::time::timeout(READ_TIMEOUT, self.stream.read_exact(&mut buf))
            .await
            .context("RCON read timed out")?
            .context("reading RCON packet body")?;
        let request_id = i32::from_le_bytes(buf[0..4].try_into().unwrap());
        // let ptype = i32::from_le_bytes(buf[4..8].try_into().unwrap());
        let payload = buf[8..buf.len() - 2].to_vec();
        Ok(Packet {
            request_id,
            payload,
        })
    }
}

/// Strip Minecraft color/formatting codes (`\u{a7}` + one char) from text.
/// Keeps command output readable in the panel UI.
pub fn strip_minecraft_colors(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{a7}' {
            chars.next(); // skip the color code character
        } else {
            out.push(c);
        }
    }
    out
}

/// True when the error means the TCP connection is dead (pool should drop
/// and reconnect). Command-level failures (bad response, auth) keep the
/// pooled connection — it isn't the connection's fault.
pub fn is_dead_connection(e: &anyhow::Error) -> bool {
    if let Some(ioe) = e.downcast_ref::<std::io::Error>() {
        matches!(
            ioe.kind(),
            std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::TimedOut
        )
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_colors() {
        assert_eq!(
            strip_minecraft_colors("\u{a7}a20.0\u{a7}r, \u{a7}bhello"),
            "20.0, hello"
        );
    }

    #[test]
    fn strips_nothing_without_codes() {
        assert_eq!(strip_minecraft_colors("plain text"), "plain text");
    }

    #[test]
    fn dead_connection_classification() {
        let e = anyhow::anyhow!(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset"
        ));
        assert!(is_dead_connection(&e));
        let e = anyhow::anyhow!("some protocol complaint");
        assert!(!is_dead_connection(&e));
    }
}
