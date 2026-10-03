<p align="center">
  <img src="assets/logo.webp" width="160" alt="forge-panel logo">
</p>

<h1 align="center">forge-panel</h1>

<p align="center"><i>Self-hosted Minecraft server management panel — AMP/FORK.gg-style control for your own boxes.</i></p>

<p align="center">
  <img src="https://img.shields.io/badge/version-0.1.0-ff7b2e?style=for-the-badge" alt="version 0.1.0">
  <img src="https://img.shields.io/badge/Rust-stable-dea584?style=for-the-badge&logo=rust" alt="Rust stable">
  <img src="https://img.shields.io/badge/Axum-web-6b7280?style=for-the-badge" alt="Axum">
  <img src="https://img.shields.io/badge/Minecraft-26.x-2f9e6e?style=for-the-badge" alt="Minecraft 26.x">
  <img src="https://img.shields.io/badge/license-MIT-2563eb?style=for-the-badge" alt="MIT">
</p>

---

A single-user, self-hosted panel for running Minecraft servers. Dashboard with live system metrics, per-server consoles, player management with inventory editing, a smart config editor, one-click installs, and a plugin browser. Dark gamer-HUD UI throughout.

## Features

### Dashboard
- All servers at a glance: status, players online, per-server RAM, TPS
- System metrics: CPU load, disk storage, network in/out, total RAM allocation
- Proxy → backend topology view
- Click any server card to open it

### Server management
- **Start / stop / restart** — managed processes with pidfiles; graceful stop via stdin `stop`, RCON fallback, then SIGTERM, then SIGKILL
- **Live console** — SSE-streamed log tail with filtering and coloring; commands go straight to process stdin (no RCON needed for local servers)
- **Players** — online list, click any player for the full inspector: health, hunger, XP, gamemode, position, inventory + ender chest editing (move/swap/delete/repair), equipment, heal/feed/freeze/kick/ban, teleport, give items
- **File browser** — browse, view, and edit server files (path-traversal protected)
- **Smart config editor** — `.properties` and YAML files get toggles, numeric steppers, and dropdowns instead of raw text
- **Backups** — one-click `tar.gz` of world folders, with download and delete
- **Settings** — per-server Xms/Xmx, ports, JVM args, all editable in the UI

### Installer
- **Vanilla** (Mojang piston-meta), **Paper** + **Velocity** (PaperMC Fill v3), **Purpur** (purpurmc.org), **Spigot** (BuildTools with live log streaming)
- **Importer** — register an existing server directory without moving files
- **Velocity proxy support** — installable proxy type, `velocity.toml` generation, link backends to proxies
- **Geyser** — one-click Geyser + Floodgate install
- Every version list is fetched **live** from upstream APIs — nothing hardcoded, ever

### Plugin browser
- Per-server Plugins tab (Bukkit-family servers): search, trending, and detail on Modrinth and Spiget
- Version picker auto-filtered to the server's detected Minecraft version and loader
- One-click install, installed list with delete

### Remote servers
- Manage a server on another machine over RCON: status, players, TPS, and an RCON command terminal
- No local process, files, or backups — just the RCON link
- Add from the Install page or via `remote_host` in config

### Backend details
- **RCON connection pooling** — one persistent connection per server, no connect/disconnect spam in logs; handles UTF-8 and strips color codes
- **Port registry** — game/RCON/query ports auto-assigned (config + live bind check), conflicts rejected
- **RAM guard** — start blocked if allocation would exceed system RAM
- **Auth** — single admin, argon2-hashed password, session cookies

## Build

```bash
cargo build --release
```

## Run

```bash
./target/release/forge-panel --config forge-panel.toml
```

On first visit to the panel URL you'll set the admin password.

## Config

See `forge-panel.example.toml`. Per-server options:

| Key | Meaning |
|---|---|
| `name` | display name (letters, numbers, `-`, `_`) |
| `dir` | server directory |
| `jar` | jar filename (default `server.jar`) |
| `java` | java binary (default from `[panel] default_java`, else `java`) |
| `jvm_args` | extra JVM args |
| `server_args` | args after `-jar` (default `--nogui`) |
| `port` | game port |
| `rcon_port` / `rcon_password` | RCON for console commands + graceful stop |
| `query_port` | GS4 query port (needs `enable-query=true` in server.properties) |
| `xms_mb` / `xmx_mb` | heap limits, applied as `-Xms`/`-Xmx` |
| `role` | `server` (default) or `proxy` |
| `behind_proxy` | name of the proxy this server sits behind |
| `mc_version` | e.g. `26.2` — used to filter plugin versions |
| `remote_host` | hostname/IP for remote servers (RCON only; `rcon_port` required) |

### Remote servers

A `[[servers]]` entry with `remote_host` set is managed over RCON only. Remote entries claim no local ports, can't be started/stopped from the panel, and get an RCON command terminal instead of a log stream.

```toml
[[servers]]
name = "remote-survival"
remote_host = "192.168.1.50"
port = 25570
rcon_port = 25575
rcon_password = "example"
```

## API notes

- PaperMC Fill v3 (`fill.papermc.io/v3`) for Paper **and** Velocity — the old `api.papermc.io/v2` is sunset (HTTP 410)
- Geyser/Floodgate from `download.geysermc.org/v2`
- Modrinth `api.modrinth.com/v2` with identifying User-Agent (per their ToS)
- Spiget `api.spiget.org/v2`; paid SpigotMC resources can't be downloaded via API and are labeled as such
- Spigot installs run BuildTools (`--rev <version>`) in the background with live log streaming; needs `git` + a JDK on PATH

## Desktop app

A Tauri 2.x native shell lives in `../forge-panel-desktop/` — double-click app instead of a browser tab. Spawns/manages the backend as a sidecar, single-instance, tray icon, remembers window size/position.
