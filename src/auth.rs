use crate::AppState;
use anyhow::{Context, Result};
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::fs::File;
use std::io::Read;
use std::sync::Arc;

pub const SESSION_COOKIE: &str = "fp_session";
/// 30 days, in seconds.
pub const SESSION_TTL_SECS: i64 = 60 * 60 * 24 * 30;

pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2 hash failed: {e}"))?
        .to_string();
    Ok(hash)
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    let parsed = match PasswordHash::new(hash) {
        Ok(p) => p,
        Err(_) => return false,
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// 64 hex chars from /dev/urandom.
pub fn new_token() -> Result<String> {
    let mut f = File::open("/dev/urandom").context("opening /dev/urandom")?;
    let mut buf = [0u8; 32];
    f.read_exact(&mut buf).context("reading urandom")?;
    Ok(buf.iter().map(|b| format!("{:02x}", b)).collect())
}

/// 24-char alphanumeric secret (for generated RCON passwords).
pub fn new_secret(len: usize) -> Result<String> {
    const CHARS: &[u8] = b"abcdefghjkmnpqrstuvwxyzABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut f = File::open("/dev/urandom").context("opening /dev/urandom")?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf).context("reading urandom")?;
    Ok(buf.iter().map(|b| CHARS[(b % CHARS.len() as u8) as usize] as char).collect())
}

pub fn cookie_token(req: &Request<Body>) -> Option<String> {
    let header_val = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for part in header_val.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&format!("{}=", SESSION_COOKIE)) {
            return Some(rest.to_string());
        }
    }
    None
}

pub fn set_session_cookie(token: &str) -> String {
    format!(
        "{}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        SESSION_COOKIE, token, SESSION_TTL_SECS
    )
}

pub fn clear_session_cookie() -> String {
    format!("{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0", SESSION_COOKIE)
}

/// Middleware: requires a valid session cookie, else 401 JSON.
pub async fn require_auth(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let ok = cookie_token(&req)
        .map(|t| state.db.session_valid(&t))
        .unwrap_or(false);
    if ok {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":"unauthorized"}"#,
        )
            .into_response()
    }
}
