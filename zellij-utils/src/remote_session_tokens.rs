use crate::consts::ZELLIJ_PROJ_DIR;
use crate::shared::set_permissions;
use rusqlite::Connection;
use std::path::PathBuf;

#[derive(Debug)]
pub enum TokenError {
    Database(rusqlite::Error),
    Io(std::io::Error),
    InvalidPath,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::Database(e) => write!(f, "Database error: {}", e),
            TokenError::Io(e) => write!(f, "IO error: {}", e),
            TokenError::InvalidPath => write!(f, "Invalid path"),
        }
    }
}

impl std::error::Error for TokenError {}

impl From<rusqlite::Error> for TokenError {
    fn from(error: rusqlite::Error) -> Self {
        TokenError::Database(error)
    }
}

impl From<std::io::Error> for TokenError {
    fn from(error: std::io::Error) -> Self {
        TokenError::Io(error)
    }
}

type Result<T> = std::result::Result<T, TokenError>;

fn get_db_path() -> Result<PathBuf> {
    let data_dir = ZELLIJ_PROJ_DIR.data_dir();
    std::fs::create_dir_all(data_dir)?;
    let db_path = data_dir.join("remote_sessions.db");
    Ok(db_path)
}

fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA busy_timeout = 5000")?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS remote_sessions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            server_url TEXT UNIQUE NOT NULL,
            session_token TEXT NOT NULL,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            last_used_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )",
        [],
    )?;

    // Additive migrations. `ALTER TABLE ... ADD COLUMN` is the only
    // operation SQLite supports without dropping/recreating the table.
    // A duplicate-column error is expected on every subsequent run and is
    // treated as a no-op here.
    let add_columns = [
        "ALTER TABLE remote_sessions ADD COLUMN e2e_encrypted INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE remote_sessions ADD COLUMN cookie_name TEXT NOT NULL DEFAULT 'session_token'",
    ];
    for stmt in add_columns {
        if let Err(e) = conn.execute(stmt, []) {
            let s = e.to_string();
            if !s.contains("duplicate column name") {
                return Err(TokenError::Database(e));
            }
        }
    }

    Ok(())
}

/// Full saved record for a remote server. Includes the cookie name so the
/// relay path (which uses `relay_session`) can coexist with the local web
/// path (which uses `session_token`), and the E2E flag so the attach
/// client can refuse a silent downgrade on token reuse.
#[derive(Debug, Clone)]
pub struct SavedRemoteSession {
    pub cookie_name: String,
    pub cookie_value: String,
    pub e2e_encrypted: bool,
}

/// Save session token for a server (upsert). Back-compat shim defaulting
/// to `session_token` cookie name and `e2e_encrypted=false`. Prefer
/// [`save_remote_session`] in new code.
pub fn save_session_token(server_url: &str, session_token: &str) -> Result<()> {
    save_remote_session(server_url, "session_token", session_token, false)
}

/// Upsert a full remote-session record (cookie name + value + E2E flag).
pub fn save_remote_session(
    server_url: &str,
    cookie_name: &str,
    cookie_value: &str,
    e2e_encrypted: bool,
) -> Result<()> {
    let db_path = get_db_path()?;

    let is_new = !db_path.exists();

    let conn = Connection::open(&db_path)?;
    init_db(&conn)?;

    if is_new {
        set_permissions(&db_path, 0o600)?;
    }

    conn.execute(
        "INSERT OR REPLACE INTO remote_sessions
           (server_url, session_token, cookie_name, e2e_encrypted, last_used_at)
         VALUES (?1, ?2, ?3, ?4, CURRENT_TIMESTAMP)",
        rusqlite::params![
            server_url,
            cookie_value,
            cookie_name,
            e2e_encrypted as i64,
        ],
    )?;

    Ok(())
}

/// Get session token for a server, update last_used_at. Back-compat
/// wrapper — prefer [`get_remote_session`] in new code.
pub fn get_session_token(server_url: &str) -> Result<Option<String>> {
    Ok(get_remote_session(server_url)?.map(|s| s.cookie_value))
}

/// Get the full saved record (cookie name + value + E2E flag). Also
/// updates `last_used_at` on hit.
pub fn get_remote_session(server_url: &str) -> Result<Option<SavedRemoteSession>> {
    let db_path = get_db_path()?;

    if !db_path.exists() {
        return Ok(None);
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;

    let row = match conn.query_row(
        "SELECT session_token, cookie_name, e2e_encrypted FROM remote_sessions WHERE server_url = ?1",
        [server_url],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    ) {
        Ok(t) => Some(t),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(TokenError::Database(e)),
    };

    if let Some((cookie_value, cookie_name, e2e_encrypted)) = row {
        // Update last_used_at
        conn.execute(
            "UPDATE remote_sessions SET last_used_at = CURRENT_TIMESTAMP WHERE server_url = ?1",
            [server_url],
        )?;
        return Ok(Some(SavedRemoteSession {
            cookie_name,
            cookie_value,
            e2e_encrypted: e2e_encrypted != 0,
        }));
    }

    Ok(None)
}

/// Delete session token for a server
pub fn delete_session_token(server_url: &str) -> Result<bool> {
    let db_path = get_db_path()?;

    if !db_path.exists() {
        return Ok(false);
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;

    let rows_affected = conn.execute(
        "DELETE FROM remote_sessions WHERE server_url = ?1",
        [server_url],
    )?;

    Ok(rows_affected > 0)
}
