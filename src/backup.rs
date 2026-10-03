use crate::AppState;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// World folders to include in a backup (only ones that exist).
fn world_dirs(server_dir: &std::path::Path) -> Vec<String> {
    ["world", "world_nether", "world_the_end"]
        .iter()
        .filter(|d| server_dir.join(d).is_dir())
        .map(|d| d.to_string())
        .collect()
}

pub fn backup_dir(state: &Arc<AppState>, server: &str) -> PathBuf {
    state.data_dir.join("backups").join(server)
}

/// Create a tar.gz of the server's world folders. Returns the file name.
pub async fn create_backup(state: Arc<AppState>, server: &str) -> Result<String> {
    let cfg = {
        let c = state.config.read().unwrap();
        c.find(server)
            .cloned()
            .with_context(|| format!("unknown server '{}'", server))?
    };
    let worlds = world_dirs(&cfg.dir);
    if worlds.is_empty() {
        anyhow::bail!("no world folders found in {}", cfg.dir.display());
    }

    let dir = backup_dir(&state, server);
    std::fs::create_dir_all(&dir).context("creating backup dir")?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let file_name = format!("{}-{}.tar.gz", server, ts);
    let dest = dir.join(&file_name);

    let mut cmd = tokio::process::Command::new("tar");
    cmd.arg("-czf").arg(&dest).arg("-C").arg(&cfg.dir);
    for w in &worlds {
        cmd.arg(w);
    }
    let out = cmd.output().await.context("running tar")?;
    if !out.status.success() {
        anyhow::bail!(
            "tar failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) as i64;
    state.db.record_backup(server, &file_name, size)?;
    Ok(file_name)
}

pub fn list_backup_files(state: &Arc<AppState>, server: &str) -> Vec<BackupFile> {
    let dir = backup_dir(state, server);
    let mut files: Vec<BackupFile> = state
        .db
        .list_backups(server)
        .into_iter()
        .map(|r| BackupFile {
            name: r.file.clone(),
            size_bytes: r.size_bytes,
            created_at: r.created_at,
            on_disk: dir.join(&r.file).is_file(),
        })
        .collect();
    // Also surface tarballs on disk that aren't in the DB (e.g. placed manually).
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".tar.gz") && !files.iter().any(|f| f.name == name) {
                let size = e.metadata().map(|m| m.len()).unwrap_or(0) as i64;
                files.push(BackupFile {
                    name,
                    size_bytes: size,
                    created_at: 0,
                    on_disk: true,
                });
            }
        }
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.created_at));
    files
}

pub struct BackupFile {
    pub name: String,
    pub size_bytes: i64,
    pub created_at: i64,
    pub on_disk: bool,
}

/// Resolve a backup file name to a path, guarding against traversal.
pub fn backup_path(state: &Arc<AppState>, server: &str, file: &str) -> Result<PathBuf> {
    if file.contains('/') || file.contains('\\') || file.contains("..") {
        anyhow::bail!("bad file name");
    }
    let dir = backup_dir(state, server)
        .canonicalize()
        .context("backup dir missing")?;
    let p = dir.join(file);
    // canonicalize requires existence; check parent containment manually.
    let parent = p
        .parent()
        .and_then(|x| x.canonicalize().ok())
        .unwrap_or(dir.clone());
    if parent != dir {
        anyhow::bail!("bad file name");
    }
    if !p.is_file() {
        anyhow::bail!("backup not found");
    }
    Ok(p)
}

pub fn delete_backup(state: &Arc<AppState>, server: &str, file: &str) -> Result<()> {
    let p = backup_path(state, server, file)?;
    std::fs::remove_file(&p).context("deleting backup file")?;
    state.db.forget_backup(server, file);
    Ok(())
}
