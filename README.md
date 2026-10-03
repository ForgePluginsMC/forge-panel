# forge-panel 🔥

Self-hosted Minecraft server management panel — a focused, single-user AMP/FORK.gg-style panel for your own boxes. Written in Rust (Axum + tokio + rusqlite).

## Features

- **Dashboard** — all servers, status, players online, per-server RAM usage, system RAM bar, proxy→backend topology. Click any server card to open it.
- **Start/stop/restart** — managed processes with pidfiles; graceful stop via stdin `stop` (RCON fallback for externally-started servers), then SIGTERM, then SIGKILL
- **Live console** — SSE-streamed log tail; commands go straight to the process stdin, so local servers need no RCON at all
- **Remote servers** — manage a server on another machine over RCON: status, players, TPS, and an RCON command terminal. No local process, files, or backups — just the RCON link. Add from the Install page or via `remote_host` in config
- **RCON connection pooling** — one persistent RCON connection per server instead of one per command, so server logs stay free of connect/disconnect spam. Built-in RCON client handles UTF-8 and strips color codes (Paper's `tps` output works)
- **Players** — online list via RCON `list`, Query-protocol fallback for counts, 20s shared cache
- **File browser** — browse/view/edit server files (path-traversal protected), with a smart config editor: `.properties`/YAML settings get toggles, numeric fields, and dropdowns instead of raw text
- **Backups** — one-click `tar.gz` of world folders, download/delete
- **Installer** — Vanilla (Mojang piston-meta), Paper + Velocity (PaperMC Fill v3 API), Purpur (purpurmc.org API), Spigot (BuildTools, streamed in the UI). All version lists are fetched **live** from upstream APIs — nothing hardcoded
- **Importer** — register an existing server directory without moving files
- **Velocity proxy support** — installable proxy type, `velocity.toml` generation, link backends to proxies, topology view
- **Geyser** — one-click Geyser + Floodgate install with Floodgate auth enabled
- **Plugin browser** — per-server Plugins tab: search/trending/detail on Modrinth and Spiget, version picker auto-filtered to the server's MC version, one-click install, installed list with delete
- **RAM management** — Xms/Xmx per server (editable in UI), start blocked if allocation would exceed system RAM
- **Port registry** — game/RCON/query ports auto-assigned (config + live bind check), conflicts rejected. Remote servers claim no local ports
- **Auth** — single admin, argon2-hashed password, session cookies
- **Gamer HUD UI** — dark neon aesthetic: ember-orange + cyan glow, angular cards, pulsing status lamps, 7-segment readouts, needle gauges (RAM, TPS), LED bar graphs, bezel-framed console

### Hard rules

- The panel **refuses to manage anything on port 25565** (config load fails, installs/imports reject it, auto-assign skips it).
- No hardcoded versions anywhere: every version list and "latest" resolution is fetched live at request time.

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
| `port` | game port (**never 25565**) |
| `rcon_port` / `rcon_password` | RCON for console commands + graceful stop |
| `query_port` | GS4 query port (needs `enable-query=true` in server.properties) |
| `xms_mb` / `xmx_mb` | heap limits, applied as `-Xms`/`-Xmx` |
| `role` | `server` (default) or `proxy` |
| `behind_proxy` | name of the proxy this server sits behind |
| `mc_version` | e.g. `26.2` — used to filter plugin versions |
| `remote_host` | set for remote servers: hostname/IP of the machine it's on. The panel then uses RCON only (no local process/files/backups); `rcon_port` is required |

### Remote servers

A `[[servers]]` entry with `remote_host` set is managed over RCON only — good for boxes you don't run the panel on. Remote entries claim no local ports (so a remote game port can match a local one), can't be started/stopped from the panel, and get an RCON command terminal instead of a log stream. Add one from the Install page ("Add remote server") or by hand:

```toml
[[servers]]
name = "remote-survival"
remote_host = "192.168.1.50"
port = 25570
rcon_port = 25575
rcon_password = "example"
```

## API notes

- PaperMC Fill v3 (`fill.papermc.io/v3`) is used for Paper **and** Velocity — the old `api.papermc.io/v2` is sunset (HTTP 410).
- Geyser/Floodgate come from `download.geysermc.org/v2` (`.../versions/latest/builds/latest/downloads/spigot`).
- Modrinth `api.modrinth.com/v2` with an identifying User-Agent (required by their ToS).
- Spiget `api.spiget.org/v2`; paid SpigotMC resources can't be downloaded via API and are labeled as such.
- Spigot installs run BuildTools (`--rev <version>`) in the background with live log streaming; needs `git` + a JDK on PATH.
