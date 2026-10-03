use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Db {
    conn: Mutex<Connection>,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening sqlite db {}", path.display()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS users (
                id INTEGER PRIMARY KEY,
                username TEXT NOT NULL UNIQUE,
                password_hash TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS sessions (
                token TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS backups (
                id INTEGER PRIMARY KEY,
                server TEXT NOT NULL,
                file TEXT NOT NULL,
                size_bytes INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            );",
        )
        .context("creating tables")?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    pub fn user_count(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
            .context("counting users")?;
        Ok(n)
    }

    pub fn create_user(&self, username: &str, password_hash: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO users (username, password_hash, created_at) VALUES (?1, ?2, ?3)",
            params![username, password_hash, now_secs()],
        )
        .context("creating user")?;
        Ok(())
    }

    pub fn password_hash(&self, username: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT password_hash FROM users WHERE username = ?1")
            .context("preparing lookup")?;
        let mut rows = stmt.query(params![username]).context("querying user")?;
        if let Some(row) = rows.next().context("reading row")? {
            Ok(Some(row.get(0).context("reading hash")?))
        } else {
            Ok(None)
        }
    }

    pub fn create_session(&self, token: &str, ttl_secs: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = now_secs();
        conn.execute(
            "INSERT INTO sessions (token, created_at, expires_at) VALUES (?1, ?2, ?3)",
            params![token, now, now + ttl_secs],
        )
        .context("creating session")?;
        Ok(())
    }

    pub fn session_valid(&self, token: &str) -> bool {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return false,
        };
        // Opportunistically purge expired sessions.
        let _ = conn.execute(
            "DELETE FROM sessions WHERE expires_at < ?1",
            params![now_secs()],
        );
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE token = ?1 AND expires_at >= ?2",
                params![token, now_secs()],
                |r| r.get(0),
            )
            .unwrap_or(0);
        count > 0
    }

    pub fn delete_session(&self, token: &str) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute("DELETE FROM sessions WHERE token = ?1", params![token]);
        }
    }

    pub fn record_backup(&self, server: &str, file: &str, size_bytes: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO backups (server, file, size_bytes, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![server, file, size_bytes, now_secs()],
        )
        .context("recording backup")?;
        Ok(())
    }

    pub fn forget_backup(&self, server: &str, file: &str) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                "DELETE FROM backups WHERE server = ?1 AND file = ?2",
                params![server, file],
            );
        }
    }

    pub fn list_backups(&self, server: &str) -> Vec<BackupRecord> {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let mut stmt = match conn.prepare(
            "SELECT file, size_bytes, created_at FROM backups WHERE server = ?1 ORDER BY created_at DESC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map(params![server], |r| {
            Ok(BackupRecord {
                file: r.get(0)?,
                size_bytes: r.get(1)?,
                created_at: r.get(2)?,
            })
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.filter_map(|r| r.ok()).collect()
    }
}

#[derive(Debug, Clone)]
pub struct BackupRecord {
    pub file: String,
    pub size_bytes: i64,
    pub created_at: i64,
}
