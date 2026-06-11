//! Relay-side tunnel auth token store (Phase 6 Session C).
//!
//! Shared-secret authentication for `zellij-relay` tunnel establishment.
//! Only SHA-256 hashes and metadata are persisted — the raw token is
//! printed once at creation time and never written to disk. Modelled on
//! `zellij-utils::web_authentication_tokens` (viewer-side auth); the two
//! namespaces are deliberately disjoint, so this module uses the long-form
//! `relay_tunnel_auth_token*` naming throughout.

use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const DEFAULT_DATA_DIR: &str = "/var/lib/zellij-relay";
pub const ENV_DATA_DIR: &str = "RELAY_DATA_DIR";
pub const DB_FILENAME: &str = "relay_tunnel_auth_tokens.db";
/// Filename used for debug-build fallback to the user's Zellij data dir.
/// Mirrors the `tokens_for_dev.db` convention in
/// `zellij-utils::web_authentication_tokens` so a dev workflow does not
/// need root permissions on `/var/lib/`.
pub const DB_FILENAME_DEV: &str = "relay_tunnel_auth_tokens_for_dev.db";

/// Type-safe hash wrapper so call sites cannot accidentally cross the
/// relay-tunnel-auth-token namespace with the viewer-auth or
/// session-token namespaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayTunnelAuthTokenHash(pub String);

#[derive(Debug)]
pub struct RelayTunnelAuthTokenInfo {
    pub label: Option<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

#[derive(Debug)]
pub enum RelayTunnelAuthTokenError {
    Database(rusqlite::Error),
    Io(std::io::Error),
    DuplicateLabel(String),
    TokenNotFound(String),
}

impl std::fmt::Display for RelayTunnelAuthTokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RelayTunnelAuthTokenError::Database(e) => write!(f, "Database error: {}", e),
            RelayTunnelAuthTokenError::Io(e) => write!(f, "IO error: {}", e),
            RelayTunnelAuthTokenError::DuplicateLabel(label) => {
                write!(f, "Relay tunnel auth token label '{}' already exists", label)
            },
            RelayTunnelAuthTokenError::TokenNotFound(id) => {
                write!(f, "Relay tunnel auth token '{}' not found", id)
            },
        }
    }
}

impl std::error::Error for RelayTunnelAuthTokenError {}

impl From<rusqlite::Error> for RelayTunnelAuthTokenError {
    fn from(error: rusqlite::Error) -> Self {
        RelayTunnelAuthTokenError::Database(error)
    }
}

impl From<std::io::Error> for RelayTunnelAuthTokenError {
    fn from(error: std::io::Error) -> Self {
        RelayTunnelAuthTokenError::Io(error)
    }
}

type Result<T> = std::result::Result<T, RelayTunnelAuthTokenError>;

/// Resolve the directory that holds the relay tunnel auth token DB.
///
/// - If `RELAY_DATA_DIR` is set, honour it verbatim (production /
///   container deployments).
/// - Otherwise, in debug builds (`cargo run -p zellij-relay`) fall back
///   to the user's Zellij data dir so a dev workflow doesn't need root
///   on `/var/lib/`.
/// - Release builds without `RELAY_DATA_DIR` default to
///   `/var/lib/zellij-relay/` — the documented production path.
fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(ENV_DATA_DIR) {
        return PathBuf::from(dir);
    }
    if cfg!(debug_assertions) {
        if let Some(proj) =
            directories::ProjectDirs::from("org", "Zellij Contributors", "Zellij")
        {
            return proj.data_dir().to_path_buf();
        }
    }
    PathBuf::from(DEFAULT_DATA_DIR)
}

fn db_filename() -> &'static str {
    if std::env::var_os(ENV_DATA_DIR).is_some() {
        DB_FILENAME
    } else if cfg!(debug_assertions) {
        DB_FILENAME_DEV
    } else {
        DB_FILENAME
    }
}

fn db_path() -> Result<PathBuf> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(db_filename()))
}

fn open_db() -> Result<Connection> {
    let path = db_path()?;
    let conn = Connection::open(&path)?;
    init_db(&conn)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(&path, perms);
    }

    Ok(conn)
}

fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA busy_timeout = 5000")?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS relay_tunnel_auth_tokens (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            token_hash TEXT UNIQUE NOT NULL,
            label TEXT UNIQUE,
            created_at INTEGER NOT NULL,
            last_used_at INTEGER
        )",
        [],
    )?;
    Ok(())
}

pub fn hash_relay_tunnel_auth_token(token: &str) -> RelayTunnelAuthTokenHash {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    RelayTunnelAuthTokenHash(format!("{:x}", hasher.finalize()))
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Creates a new 128-bit tunnel-auth token, stores only its hash, and
/// returns the raw token (once — the operator must copy it immediately).
pub fn store_new_relay_tunnel_auth_token(label: Option<String>) -> Result<String> {
    let conn = open_db()?;
    let token = Uuid::new_v4().to_string();
    let hash = hash_relay_tunnel_auth_token(&token).0;
    let created_at = now_epoch();

    match conn.execute(
        "INSERT INTO relay_tunnel_auth_tokens (token_hash, label, created_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![&hash, &label, created_at],
    ) {
        Err(rusqlite::Error::SqliteFailure(ffi_error, _))
            if ffi_error.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Err(RelayTunnelAuthTokenError::DuplicateLabel(
                label.unwrap_or_else(|| "<unnamed>".into()),
            ))
        },
        Err(e) => Err(RelayTunnelAuthTokenError::Database(e)),
        Ok(_) => Ok(token),
    }
}

/// Checks whether `hash` matches any known token. On hit, refreshes
/// `last_used_at` to the current epoch and returns `true`.
pub fn validate_relay_tunnel_auth_token_hash(hash: &RelayTunnelAuthTokenHash) -> Result<bool> {
    if hash.0.is_empty() {
        return Ok(false);
    }
    let conn = open_db()?;
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM relay_tunnel_auth_tokens WHERE token_hash = ?1",
        [&hash.0],
        |row| row.get(0),
    )?;
    if count == 0 {
        return Ok(false);
    }
    let now = now_epoch();
    conn.execute(
        "UPDATE relay_tunnel_auth_tokens SET last_used_at = ?1 WHERE token_hash = ?2",
        rusqlite::params![now, &hash.0],
    )?;
    Ok(true)
}

/// Revokes by label (preferred) or by a plaintext token if the caller has it.
/// Returns the number of rows deleted.
pub fn revoke_relay_tunnel_auth_token(label_or_token: &str) -> Result<usize> {
    let conn = open_db()?;
    // First try by label.
    let by_label = conn.execute(
        "DELETE FROM relay_tunnel_auth_tokens WHERE label = ?1",
        [&label_or_token],
    )?;
    if by_label > 0 {
        return Ok(by_label);
    }
    // Fall back to treating the argument as a raw token.
    let hash = hash_relay_tunnel_auth_token(label_or_token).0;
    let by_hash = conn.execute(
        "DELETE FROM relay_tunnel_auth_tokens WHERE token_hash = ?1",
        [&hash],
    )?;
    if by_hash == 0 {
        return Err(RelayTunnelAuthTokenError::TokenNotFound(
            label_or_token.to_string(),
        ));
    }
    Ok(by_hash)
}

pub fn list_relay_tunnel_auth_tokens() -> Result<Vec<RelayTunnelAuthTokenInfo>> {
    let conn = open_db()?;
    let mut stmt = conn.prepare(
        "SELECT label, created_at, last_used_at FROM relay_tunnel_auth_tokens ORDER BY created_at",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(RelayTunnelAuthTokenInfo {
            label: row.get::<_, Option<String>>(0)?,
            created_at: row.get::<_, i64>(1)?,
            last_used_at: row.get::<_, Option<i64>>(2)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Points the token store at a per-test scratch dir so the real
    /// `/var/lib/zellij-relay` is never touched.
    fn with_scratch_dir<F: FnOnce()>(f: F) {
        let tmp = std::env::temp_dir().join(format!("zellij-relay-auth-{}", Uuid::new_v4()));
        std::env::set_var(ENV_DATA_DIR, &tmp);
        f();
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var(ENV_DATA_DIR);
    }

    #[test]
    #[serial]
    fn empty_hash_is_rejected() {
        with_scratch_dir(|| {
            let result =
                validate_relay_tunnel_auth_token_hash(&RelayTunnelAuthTokenHash(String::new()))
                    .unwrap();
            assert!(!result);
        });
    }

    #[test]
    #[serial]
    fn unknown_hash_is_rejected() {
        with_scratch_dir(|| {
            let _ = open_db().unwrap();
            let result = validate_relay_tunnel_auth_token_hash(&RelayTunnelAuthTokenHash(
                "deadbeef".into(),
            ))
            .unwrap();
            assert!(!result);
        });
    }

    #[test]
    #[serial]
    fn stored_token_validates_and_updates_last_used() {
        with_scratch_dir(|| {
            let token = store_new_relay_tunnel_auth_token(Some("laptop".into())).unwrap();
            let hash = hash_relay_tunnel_auth_token(&token);
            assert!(validate_relay_tunnel_auth_token_hash(&hash).unwrap());
            let tokens = list_relay_tunnel_auth_tokens().unwrap();
            assert_eq!(tokens.len(), 1);
            assert_eq!(tokens[0].label.as_deref(), Some("laptop"));
            assert!(tokens[0].last_used_at.is_some());
        });
    }

    #[test]
    #[serial]
    fn duplicate_label_is_rejected() {
        with_scratch_dir(|| {
            let _ = store_new_relay_tunnel_auth_token(Some("laptop".into())).unwrap();
            let err = store_new_relay_tunnel_auth_token(Some("laptop".into())).unwrap_err();
            assert!(matches!(
                err,
                RelayTunnelAuthTokenError::DuplicateLabel(_)
            ));
        });
    }

    #[test]
    #[serial]
    fn revoke_by_label_removes_token() {
        with_scratch_dir(|| {
            let token = store_new_relay_tunnel_auth_token(Some("laptop".into())).unwrap();
            let hash = hash_relay_tunnel_auth_token(&token);
            assert_eq!(revoke_relay_tunnel_auth_token("laptop").unwrap(), 1);
            assert!(!validate_relay_tunnel_auth_token_hash(&hash).unwrap());
        });
    }

    #[test]
    #[serial]
    fn revoke_by_raw_token_removes_token() {
        with_scratch_dir(|| {
            let token = store_new_relay_tunnel_auth_token(None).unwrap();
            let hash = hash_relay_tunnel_auth_token(&token);
            assert_eq!(revoke_relay_tunnel_auth_token(&token).unwrap(), 1);
            assert!(!validate_relay_tunnel_auth_token_hash(&hash).unwrap());
        });
    }

    #[test]
    #[serial]
    fn revoke_missing_token_errors() {
        with_scratch_dir(|| {
            let err = revoke_relay_tunnel_auth_token("nope").unwrap_err();
            assert!(matches!(
                err,
                RelayTunnelAuthTokenError::TokenNotFound(_)
            ));
        });
    }
}
