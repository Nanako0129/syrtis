//! Read-only access to the Cursor desktop app's login in `state.vscdb`.
//!
//! Two consumers share this reader: the Grok Bot quota fallback
//! (`agent_grokbot`) and the Cursor usage sync (`cursor_sync`). The database
//! is opened read-only, never written, and the token is returned to the
//! caller only; nothing here logs or persists it.
//!
//! `TOKENBAR_CURSOR_STATE_VSCDB` overrides the database path (tests,
//! debugging). It selects which file is read, never where anything is sent.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `state.vscdb` can be briefly locked by a running Cursor; wait rather than
/// fail the poll.
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_millis(3000);

/// The three rows a login is made of. Each user id is `None` when its row is
/// absent or holds no recognisable `user_...` id.
pub(crate) struct CursorLogin {
    pub access_token: String,
    pub glass_user_id: Option<String>,
    pub profile_user_id: Option<String>,
}

impl CursorLogin {
    /// The stored account id: `glass.lastSignedInAuthId`, else the cached
    /// profile. Grok Bot's cookie is keyed by this.
    pub(crate) fn stored_user_id(&self) -> Option<&str> {
        self.glass_user_id
            .as_deref()
            .or(self.profile_user_id.as_deref())
    }
}

pub(crate) fn state_db_path() -> PathBuf {
    if let Some(path) = std::env::var_os("TOKENBAR_CURSOR_STATE_VSCDB").filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    // `crate::user_home_dir`, not `dirs::home_dir`: a non-empty `HOME` wins
    // over the platform account directory, and this path selects which
    // account's Cursor login is read.
    crate::user_home_dir()
        .map(|home| home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb"))
        .unwrap_or_else(|| PathBuf::from("state.vscdb"))
}

/// `Ok(None)` when the database is absent or holds no access token.
pub(crate) fn read_login(db_path: &Path) -> Result<Option<CursorLogin>, String> {
    if !db_path.is_file() {
        return Ok(None);
    }
    let unreadable = |_| "Could not read the Cursor login database.".to_string();
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(unreadable)?;
    conn.busy_timeout(SQLITE_BUSY_TIMEOUT).map_err(unreadable)?;

    let mut stmt = conn
        .prepare(
            "SELECT key, value FROM ItemTable WHERE key IN \
             ('cursorAuth/accessToken','glass.lastSignedInAuthId','cursorAuth/cachedScopedProfile')",
        )
        .map_err(unreadable)?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(unreadable)?
        .collect::<Result<_, _>>()
        .map_err(unreadable)?;

    let mut token: Option<String> = None;
    let mut identity: Option<String> = None;
    let mut profile: Option<String> = None;
    for (key, value) in &rows {
        let normalized = normalized_stored_string(value);
        match key.as_str() {
            "cursorAuth/accessToken" => token = normalized,
            "glass.lastSignedInAuthId" => identity = normalized,
            "cursorAuth/cachedScopedProfile" => profile = normalized,
            _ => {}
        }
    }

    let Some(access_token) = token.filter(|t| !t.is_empty()) else {
        return Ok(None);
    };
    Ok(Some(CursorLogin {
        access_token,
        glass_user_id: identity.as_deref().and_then(extract_user_id),
        profile_user_id: profile.as_deref().and_then(extract_user_id),
    }))
}

/// Values in `state.vscdb` are sometimes JSON-encoded strings (wrapped in an
/// extra layer of quotes) — unwrap one layer when present, else use as-is.
pub(crate) fn normalized_stored_string(value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    if value.starts_with('"') {
        if let Ok(Value::String(inner)) = serde_json::from_str(value) {
            return if inner.is_empty() { None } else { Some(inner) };
        }
    }
    Some(value.to_string())
}

/// Cursor user ids look like `user_...` (20+ alphanumerics). Scan for the
/// first occurrence rather than depending on the surrounding JSON shape.
pub(crate) fn extract_user_id(text: &str) -> Option<String> {
    let start = text.find("user_")?;
    let rest = &text[start + "user_".len()..];
    let len = rest
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_alphanumeric())
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    (len >= 20).then(|| format!("user_{}", &rest[..len]))
}
