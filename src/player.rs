//! Per-player info and inventory, fetched over RCON with `data get`.
//!
//! Powers the Players tab: click a player for their skin, stats, inventory,
//! enderchest, and moderation actions. Needs RCON (unlike the stdin console,
//! `data get` requires a command *response*).

use crate::mc::RconPool;
use anyhow::{bail, Context, Result};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ItemStack {
    pub slot: i32,
    pub id: String,
    pub count: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlayerInfo {
    pub name: String,
    pub uuid: Option<String>,
    pub health: Option<f32>,
    pub food: Option<i32>,
    pub saturation: Option<f32>,
    pub xp_level: Option<i32>,
    pub xp_total: Option<i32>,
    pub gamemode: Option<i32>,
    pub dimension: Option<String>,
    pub pos: Option<(f64, f64, f64)>,
    pub inventory: Vec<ItemStack>,
    pub ender: Vec<ItemStack>,
}

/// Player names as the server knows them. Allows Geyser-style names
/// (spaces, dots) while blocking anything that could break out of the
/// `data get entity <name>` command.
pub fn valid_player_name(name: &str) -> bool {
    let n = name.chars().count();
    n >= 1
        && n <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ' ' | '.' | '-'))
}

/// Run `data get entity <player> [path]` and return the raw NBT-ish value.
async fn data_get(
    pool: &RconPool,
    host: &str,
    port: u16,
    pw: &str,
    player: &str,
    path: &str,
) -> Result<String> {
    let cmd = if path.is_empty() {
        format!("data get entity {}", player)
    } else {
        format!("data get entity {} {}", player, path)
    };
    let out = pool
        .run(host, port, pw, &cmd)
        .await
        .with_context(|| format!("data get {}", path))?;
    match out.split_once("has the following entity data: ") {
        Some((_, data)) => Ok(data.trim().to_string()),
        None => {
            if out.contains("No entity was found") {
                bail!("player not found (are they online?)");
            }
            bail!(
                "unexpected response: {}",
                out.chars().take(80).collect::<String>()
            );
        }
    }
}

fn parse_float(s: &str) -> Option<f32> {
    s.trim().trim_end_matches(['f', 'd', 'F', 'D']).parse().ok()
}

fn parse_int(s: &str) -> Option<i32> {
    let t = s.trim();
    // NBT numeric suffixes: 20, 20b, 20s, 20l — keep leading minus.
    let num: String = t
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    if num.is_empty() || num == "-" {
        return None;
    }
    num.parse().ok()
}

fn parse_uuid(s: &str) -> Option<String> {
    // [I; 123, -456, 789, 101112]
    let inner = s.trim().strip_prefix("[I;")?.strip_suffix(']')?;
    let parts: Vec<i32> = inner
        .split(',')
        .map(|p| p.trim().parse().ok())
        .collect::<Option<Vec<_>>>()?;
    if parts.len() != 4 {
        return None;
    }
    let msb = ((parts[0] as u64) << 32) | (parts[1] as u32 as u64);
    let lsb = ((parts[2] as u64) << 32) | (parts[3] as u32 as u64);
    Some(format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (msb >> 32) as u32,
        ((msb >> 16) & 0xffff) as u32,
        (msb & 0xffff) as u32,
        ((lsb >> 48) & 0xffff) as u32,
        lsb & 0xffff_ffff_ffff,
    ))
}

fn parse_pos(s: &str) -> Option<(f64, f64, f64)> {
    // [12.5d, 64.0d, -30.2d]
    let inner = s.trim().strip_prefix('[')?.strip_suffix(']')?;
    let parts: Vec<f64> = inner
        .split(',')
        .map(|p| p.trim().trim_end_matches(['d', 'D']).parse().ok())
        .collect::<Option<Vec<_>>>()?;
    if parts.len() != 3 {
        return None;
    }
    Some((parts[0], parts[1], parts[2]))
}

/// Find `key:` at brace-depth 1 in a `{...}` compound and return the raw
/// value text (up to the next top-level `,` or `}`).
fn extract_field<'a>(compound: &'a str, key: &str) -> Option<&'a str> {
    let bytes = compound.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth -= 1,
            _ => {}
        }
        if depth == 1 && compound[i..].starts_with(key) {
            if let Some(rest) = compound[i + key.len()..].strip_prefix(':') {
                let rest = rest.trim_start();
                let rb = rest.as_bytes();
                let mut j = 0;
                let mut d = 1i32;
                let mut s2 = false;
                while j < rb.len() {
                    let cc = rb[j];
                    if s2 {
                        if cc == b'"' {
                            s2 = false;
                        }
                    } else if cc == b'"' {
                        s2 = true;
                    } else if cc == b'{' || cc == b'[' {
                        d += 1;
                    } else if cc == b'}' || cc == b']' {
                        d -= 1;
                        if d == 0 {
                            break;
                        }
                    } else if cc == b',' && d == 1 {
                        break;
                    }
                    j += 1;
                }
                return Some(rest[..j].trim());
            }
        }
        i += 1;
    }
    None
}

fn parse_item(compound: &str) -> Option<ItemStack> {
    let slot = parse_int(extract_field(compound, "Slot")?)?;
    let id = extract_field(compound, "id")?;
    let id = id.strip_prefix('"')?.strip_suffix('"')?.to_string();
    // 1.20.5+ item components use lowercase `count`; older NBT used `Count`.
    let count_raw = extract_field(compound, "Count")
        .or_else(|| extract_field(compound, "count"))?;
    let count = parse_int(count_raw)?;
    Some(ItemStack { slot, id, count })
}

/// Parse `[{Slot: 0b, id: "minecraft:stone", Count: 64b}, ...]`.
/// Brace-depth aware so nested `tag` compounds don't confuse it.
fn parse_items(s: &str) -> Vec<ItemStack> {
    let mut items = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let mut depth = 0i32;
            let mut in_str = false;
            let mut j = i;
            while j < bytes.len() {
                let c = bytes[j];
                if in_str {
                    if c == b'"' {
                        in_str = false;
                    }
                } else if c == b'"' {
                    in_str = true;
                } else if c == b'{' {
                    depth += 1;
                } else if c == b'}' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                j += 1;
            }
            if j < bytes.len() {
                if let Some(item) = parse_item(&s[i..=j]) {
                    items.push(item);
                }
                i = j + 1;
                continue;
            }
            break;
        }
        i += 1;
    }
    items.sort_by_key(|it| it.slot);
    items
}

/// Parse `banlist players` output:
/// "There are 2 banned players: alice, bob" / "There are no banned players".
pub fn parse_banlist(s: &str) -> Vec<String> {
    let s = s.trim();
    if s.contains("no banned") {
        return Vec::new();
    }
    match s.split_once("banned players:") {
        Some((_, names)) => names
            .split(',')
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect(),
        None => Vec::new(),
    }
}

pub fn gamemode_name(gm: i32) -> &'static str {
    match gm {
        0 => "Survival",
        1 => "Creative",
        2 => "Adventure",
        3 => "Spectator",
        _ => "Unknown",
    }
}

/// Fetch everything for the player panel. The first `data get` must succeed
/// (it proves the player is online); the rest are best-effort.
pub async fn fetch_player(
    pool: &RconPool,
    host: &str,
    port: u16,
    pw: &str,
    player: &str,
) -> Result<PlayerInfo> {
    if !valid_player_name(player) {
        bail!("invalid player name");
    }
    // Proves they're online; propagates "player not found".
    let health_raw = data_get(pool, host, port, pw, player, "Health").await?;

    let uuid = data_get(pool, host, port, pw, player, "UUID")
        .await
        .ok()
        .and_then(|s| parse_uuid(&s));
    let food = data_get(pool, host, port, pw, player, "foodLevel")
        .await
        .ok()
        .and_then(|s| parse_int(&s));
    let saturation = data_get(pool, host, port, pw, player, "foodSaturationLevel")
        .await
        .ok()
        .and_then(|s| parse_float(&s));
    let xp_level = data_get(pool, host, port, pw, player, "XpLevel")
        .await
        .ok()
        .and_then(|s| parse_int(&s));
    let xp_total = data_get(pool, host, port, pw, player, "XpTotal")
        .await
        .ok()
        .and_then(|s| parse_int(&s));
    let gamemode = data_get(pool, host, port, pw, player, "playerGameType")
        .await
        .ok()
        .and_then(|s| parse_int(&s));
    let dimension = data_get(pool, host, port, pw, player, "Dimension")
        .await
        .ok()
        .map(|s| s.trim().trim_matches('"').to_string());
    let pos = data_get(pool, host, port, pw, player, "Pos")
        .await
        .ok()
        .and_then(|s| parse_pos(&s));
    let inventory = data_get(pool, host, port, pw, player, "Inventory")
        .await
        .ok()
        .map(|s| parse_items(&s))
        .unwrap_or_default();
    let ender = data_get(pool, host, port, pw, player, "EnderItems")
        .await
        .ok()
        .map(|s| parse_items(&s))
        .unwrap_or_default();

    Ok(PlayerInfo {
        name: player.to_string(),
        uuid,
        health: parse_float(&health_raw),
        food,
        saturation,
        xp_level,
        xp_total,
        gamemode,
        dimension,
        pos,
        inventory,
        ender,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_inventory() {
        let raw = r#"[{Slot: 0b, id: "minecraft:diamond_sword", Count: 1b, tag: {Damage: 10}}, {Slot: 9b, id: "minecraft:stone", Count: 64b}, {Slot: -106b, id: "minecraft:shield", Count: 1b}]"#;
        let items = parse_items(raw);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].slot, -106);
        assert_eq!(items[0].id, "minecraft:shield");
        assert_eq!(items[1].slot, 0);
        assert_eq!(items[1].id, "minecraft:diamond_sword");
        assert_eq!(items[1].count, 1);
        assert_eq!(items[2].slot, 9);
        assert_eq!(items[2].count, 64);
        // 1.20.5+ uses lowercase `count` without a suffix.
        let raw2 = r#"[{Slot: 0b, id: "minecraft:dirt", count: 5}]"#;
        let items2 = parse_items(raw2);
        assert_eq!(items2.len(), 1);
        assert_eq!(items2[0].count, 5);
    }

    #[test]
    fn parses_uuid() {
        let u = parse_uuid("[I; 123, -456, 789, 101112]").unwrap();
        assert_eq!(u.len(), 36);
        assert_eq!(u.chars().filter(|c| *c == '-').count(), 4);
    }

    #[test]
    fn parses_pos() {
        let (x, y, z) = parse_pos("[12.5d, 64.0d, -30.25d]").unwrap();
        assert!((x - 12.5).abs() < 1e-9 && (y - 64.0).abs() < 1e-9 && (z + 30.25).abs() < 1e-9);
    }

    #[test]
    fn parses_banlist() {
        assert_eq!(parse_banlist("There are 2 banned players: alice, bob"), vec!["alice", "bob"]);
        assert!(parse_banlist("There are no banned players").is_empty());
        assert!(parse_banlist("garbage").is_empty());
    }

    #[test]
    fn name_validation() {
        assert!(valid_player_name("chris_irwin"));
        assert!(valid_player_name("Dream"));
        assert!(valid_player_name("a b")); // geyser-style
        assert!(!valid_player_name(""));
        assert!(!valid_player_name("a\"; rm -rf"));
        assert!(!valid_player_name("x".repeat(33).as_str()));
    }
}
