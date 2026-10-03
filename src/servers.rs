use crate::config::ServerConfig;
use crate::mc::RconPool;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};

/// Runtime state for panel-managed processes.
pub struct ServerRuntime {
    pub child: Option<Child>,
    /// Piped stdin of the child, when the panel started it. Console commands
    /// go straight here — local servers never need RCON for console input.
    /// (Arc so the map lock is never held across an `.await`.)
    pub stdin: Option<Arc<tokio::sync::Mutex<ChildStdin>>>,
}

impl ServerRuntime {
    pub fn new() -> Self {
        ServerRuntime {
            child: None,
            stdin: None,
        }
    }
}

/// Total system RAM in MB, from /proc/meminfo.
pub fn system_ram_mb() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest
                .split_whitespace()
                .next()?
                .parse()
                .ok()?;
            return Some(kb / 1024);
        }
    }
    None
}

/// RSS of a process in MB, from /proc/<pid>/status.
pub fn process_rss_mb(pid: u32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb / 1024);
        }
    }
    None
}

pub struct ServerManager {
    runtimes: RwLock<HashMap<String, ServerRuntime>>,
    data_dir: PathBuf,
    rcon: Arc<RconPool>,
}

impl ServerManager {
    pub fn new(data_dir: PathBuf, rcon: Arc<RconPool>) -> Self {
        ServerManager {
            runtimes: RwLock::new(HashMap::new()),
            data_dir,
            rcon,
        }
    }

    fn pidfile(&self, name: &str) -> PathBuf {
        self.data_dir.join("pids").join(format!("{}.pid", name))
    }

    fn write_pidfile(&self, name: &str, pid: u32) -> Result<()> {
        let dir = self.data_dir.join("pids");
        std::fs::create_dir_all(&dir).context("creating pids dir")?;
        std::fs::write(self.pidfile(name), pid.to_string()).context("writing pidfile")?;
        Ok(())
    }

    fn remove_pidfile(&self, name: &str) {
        let _ = std::fs::remove_file(self.pidfile(name));
    }

    /// Is a pid alive AND does its cmdline mention this jar? Guards against pid reuse.
    fn pid_is_ours(pid: u32, jar: &str) -> bool {
        let cmdline = std::fs::read_to_string(format!("/proc/{}/cmdline", pid)).unwrap_or_default();
        !cmdline.is_empty() && cmdline.replace('\0', " ").contains(jar)
    }

    pub fn is_running(&self, cfg: &ServerConfig) -> bool {
        // 1. Live child handle owned by this panel process.
        {
            let runtimes = self.runtimes.read().unwrap();
            if let Some(rt) = runtimes.get(&cfg.name) {
                if rt.child.is_some() {
                    return true;
                }
            }
        }
        // 2. Pidfile from a previous panel run (or external start).
        if let Ok(text) = std::fs::read_to_string(self.pidfile(&cfg.name)) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                return Self::pid_is_ours(pid, &cfg.jar);
            }
        }
        false
    }

    /// Pid of the running process, if any (child handle or pidfile).
    pub fn running_pid(&self, cfg: &ServerConfig) -> Option<u32> {
        {
            let runtimes = self.runtimes.read().unwrap();
            if let Some(rt) = runtimes.get(&cfg.name) {
                if let Some(child) = &rt.child {
                    return child.id();
                }
            }
        }
        if let Ok(text) = std::fs::read_to_string(self.pidfile(&cfg.name)) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                if Self::pid_is_ours(pid, &cfg.jar) {
                    return Some(pid);
                }
            }
        }
        None
    }

    /// RSS in MB of the running server, if it's up and readable.
    pub fn memory_mb(&self, cfg: &ServerConfig) -> Option<u64> {
        self.running_pid(cfg).and_then(process_rss_mb)
    }

    /// Block starting `cfg` if its Xmx plus the Xmx of every other running
    /// server would exceed system RAM. Only enforced when every running
    /// server (and the new one) has an Xmx set; otherwise we can't sum.
    pub fn check_ram(&self, all: &[ServerConfig], cfg: &ServerConfig) -> Result<()> {
        let want = match cfg.xmx_mb {
            Some(m) => m as u64,
            None => return Ok(()), // nothing to enforce
        };
        let total = match system_ram_mb() {
            Some(t) => t,
            None => return Ok(()), // can't read meminfo; don't block
        };
        let mut sum = want;
        for other in all {
            if other.name == cfg.name {
                continue;
            }
            if !self.is_running(other) {
                continue;
            }
            match other.xmx_mb {
                Some(m) => sum += m as u64,
                None => return Ok(()), // unknown allocation; can't enforce
            }
        }
        if sum > total {
            bail!(
                "not enough RAM: starting '{}' ({} MB) would allocate {} MB of {} MB system RAM",
                cfg.name,
                want,
                sum,
                total
            );
        }
        Ok(())
    }

    /// Sum of Xmx across running servers that have one set (for dashboard warnings).
    /// Returns (sum_mb, complete) where complete=false means some running
    /// server has no Xmx configured so the sum is a lower bound.
    pub fn allocated_mb(&self, all: &[ServerConfig]) -> (u64, bool) {
        let mut sum = 0u64;
        let mut complete = true;
        for s in all {
            if !self.is_running(s) {
                continue;
            }
            match s.xmx_mb {
                Some(m) => sum += m as u64,
                None => complete = false,
            }
        }
        (sum, complete)
    }

    /// Path the panel appends managed-process stdout/stderr to.
    pub fn console_log_path(cfg: &ServerConfig) -> PathBuf {
        cfg.dir.join("logs").join(format!("console-{}.log", cfg.name))
    }

    /// Canonical server log (written by Paper itself). Preferred for tailing
    /// when present; falls back to the panel-captured console log.
    pub fn tail_log_path(cfg: &ServerConfig) -> PathBuf {
        let latest = cfg.dir.join("logs").join("latest.log");
        if latest.exists() {
            latest
        } else {
            Self::console_log_path(cfg)
        }
    }

    pub async fn start(&self, cfg: &ServerConfig, java: &str, all: &[ServerConfig]) -> Result<()> {
        if self.is_running(cfg) {
            bail!("server '{}' is already running", cfg.name);
        }
        // Refuse to over-allocate system RAM.
        self.check_ram(all, cfg)?;
        let jar_path = cfg.dir.join(&cfg.jar);
        if !jar_path.exists() {
            bail!("jar not found: {}", jar_path.display());
        }
        let logs_dir = cfg.dir.join("logs");
        std::fs::create_dir_all(&logs_dir).context("creating logs dir")?;

        // Build the JVM args: explicit xms/xmx from config win over anything
        // already sitting in jvm_args.
        let mut jvm: Vec<String> = Vec::new();
        if let Some(xms) = cfg.xms_mb {
            jvm.push(format!("-Xms{}M", xms));
        }
        if let Some(xmx) = cfg.xmx_mb {
            jvm.push(format!("-Xmx{}M", xmx));
        }
        let ram_set = cfg.xms_mb.is_some() || cfg.xmx_mb.is_some();
        for a in &cfg.jvm_args {
            if ram_set && (a.starts_with("-Xms") || a.starts_with("-Xmx")) {
                continue;
            }
            jvm.push(a.clone());
        }

        let mut cmd = Command::new(java);
        cmd.current_dir(&cfg.dir);
        for a in &jvm {
            cmd.arg(a);
        }
        cmd.arg("-jar").arg(&cfg.jar);
        for a in &cfg.server_args {
            cmd.arg(a);
        }
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Piped stdin: console commands go straight to the process, so
            // local servers never need RCON for console input.
            .stdin(std::process::Stdio::piped());
        // Detach from the panel's process group so signals to the panel
        // don't propagate to the game server.
        // (tokio Command has no direct setsid; the child outlives us via
        // the OS once spawned — good enough for v1.)

        let mut child = cmd.spawn().context("spawning java process")?;
        let pid = child.id().context("child has no pid")?;

        // Pump stdout/stderr into the console log.
        let log_path = Self::console_log_path(cfg);
        if let Some(stdout) = child.stdout.take() {
            let lp = log_path.clone();
            tokio::spawn(async move {
                append_stream_to_file(stdout, &lp).await;
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let lp = log_path.clone();
            tokio::spawn(async move {
                append_stream_to_file(stderr, &lp).await;
            });
        }

        self.write_pidfile(&cfg.name, pid)?;
        let stdin = child
            .stdin
            .take()
            .map(|s| Arc::new(tokio::sync::Mutex::new(s)));
        let mut runtimes = self.runtimes.write().unwrap();
        let rt = runtimes
            .entry(cfg.name.clone())
            .or_insert_with(ServerRuntime::new);
        rt.child = Some(child);
        rt.stdin = stdin;
        Ok(())
    }

    /// Write a line to a panel-started server's stdin (console input).
    /// Fails when the panel doesn't own the process (external start, or the
    /// panel restarted since) — callers fall back to RCON then.
    async fn write_stdin(&self, name: &str, line: &str) -> Result<()> {
        // Clone the Arc out under a short lock; never hold the map guard
        // across the awaits below (keeps the future Send).
        let stdin = {
            let guard = self.runtimes.read().unwrap();
            let rt = guard
                .get(name)
                .with_context(|| format!("no runtime for '{}'", name))?;
            rt.stdin.clone()
        };
        let stdin = stdin.context("no stdin handle (server not started by this panel run)")?;
        let mut stdin = stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .context("writing to server stdin")?;
        stdin.write_all(b"\n").await.context("writing to server stdin")?;
        stdin.flush().await.context("flushing server stdin")?;
        Ok(())
    }

    /// Send a console command to a local server. Straight to the process
    /// stdin — no RCON needed.
    pub async fn send_input(&self, name: &str, command: &str) -> Result<()> {
        self.write_stdin(name, command).await
    }

    /// Graceful stop: stdin `stop` for panel-owned processes (no RCON
    /// needed), RCON `stop` for externally-started ones with RCON
    /// configured, then SIGTERM, then SIGKILL.
    pub async fn stop(&self, cfg: &ServerConfig) -> Result<()> {
        // Best-effort graceful stop without RCON when we own the process.
        if self.write_stdin(&cfg.name, "stop").await.is_err() {
            if let (Some(port), Some(pw)) = (cfg.rcon_port, cfg.rcon_password.as_deref()) {
                let _ = self.rcon.run("127.0.0.1", port, pw, "stop").await;
            }
        }

        let child_opt = {
            let mut runtimes = self.runtimes.write().unwrap();
            let mut rt = runtimes.get_mut(&cfg.name);
            if let Some(rt) = rt.as_mut() {
                rt.stdin = None;
            }
            rt.and_then(|rt| rt.child.take())
        };

        if let Some(mut child) = child_opt {
            // Give the RCON stop a moment to take effect.
            if tokio::time::timeout(Duration::from_secs(20), child.wait())
                .await
                .is_ok()
            {
                self.remove_pidfile(&cfg.name);
                return Ok(());
            }
            // SIGTERM via pid for a clean shutdown (lets the server save).
            if let Some(pid) = child.id() {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid as i32),
                    nix::sys::signal::Signal::SIGTERM,
                );
                if tokio::time::timeout(Duration::from_secs(10), child.wait())
                    .await
                    .is_ok()
                {
                    self.remove_pidfile(&cfg.name);
                    return Ok(());
                }
            }
            // Last resort.
            let _ = child.kill().await;
            let _ = child.wait().await;
            self.remove_pidfile(&cfg.name);
            return Ok(());
        }

        // No child handle: try pidfile SIGTERM (server started outside panel).
        if let Ok(text) = std::fs::read_to_string(self.pidfile(&cfg.name)) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid as i32),
                    nix::sys::signal::Signal::SIGTERM,
                );
                for _ in 0..20 {
                    if !Self::pid_is_ours(pid, &cfg.jar) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
        self.remove_pidfile(&cfg.name);
        Ok(())
    }

    pub async fn restart(&self, cfg: &ServerConfig, java: &str, all: &[ServerConfig]) -> Result<()> {
        let _ = self.stop(cfg).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        self.start(cfg, java, all).await
    }
}

async fn append_stream_to_file<R>(mut stream: R, path: &Path)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 8192];
    let open = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await;
    let mut file = match open {
        Ok(f) => f,
        Err(_) => return,
    };
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if file.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}
