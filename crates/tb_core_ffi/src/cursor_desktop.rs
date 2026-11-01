//! Read-only access to the Cursor desktop app's login in `state.vscdb`.
//!
//! Two consumers share this reader: the Grok Bot quota fallback
//! (`agent_grokbot`) and the Cursor usage sync (`cursor_sync`). The database
//! is opened read-only, never written, and the token is returned to the
//! caller only; nothing here logs or persists it.
//!
//! `TOKENBAR_CURSOR_STATE_VSCDB` overrides the default profile's database
//! path (tests, debugging). It selects which file is read, never where
//! anything is sent. Added Cursor profiles (`cursor_sync`) derive their own
//! path from the profile root, are read with `read_login_checked`, and never
//! feed Grok Bot.

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
    query_login(&conn).map_err(unreadable)
}

/// The Cursor usage sync's reader for an *added* profile (plan CMP-520,
/// per-sync DB checks): the last path component must be a regular file owned
/// by the current user, not a symlink (intermediate symlinks are allowed);
/// the open refuses a symlink (`SQLITE_OPEN_NOFOLLOW`) and runs with
/// `trusted_schema=OFF` and `DEFENSIVE`. Every refusal is a fixed reason
/// code; `Ok(None)` is an absent database or one without a token. The
/// default profile (sync and Grok Bot) keeps `read_login`, which follows a
/// symlinked `state.vscdb` as every earlier release did.
pub(crate) fn read_login_checked(db_path: &Path) -> Result<Option<CursorLogin>, &'static str> {
    #[cfg(unix)]
    let uid = current_uid();
    #[cfg(not(unix))]
    let uid = 0;
    read_login_checked_for(db_path, uid)
}

fn read_login_checked_for(
    db_path: &Path,
    #[cfg_attr(not(unix), allow(unused_variables))] uid: u32,
) -> Result<Option<CursorLogin>, &'static str> {
    let meta = match std::fs::symlink_metadata(db_path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("login_unreadable"),
    };
    if meta.file_type().is_symlink() {
        return Err("db_symlink");
    }
    if !meta.file_type().is_file() {
        return Err("db_not_file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if meta.uid() != uid {
            return Err("db_foreign_owner");
        }
    }
    let conn = open_checked(db_path)?;
    query_login(&conn).map_err(|_| "login_unreadable")
}

/// SQLite's NOFOLLOW refuses a symlink in *any* component (its full-pathname
/// resolution reports every one), so the directory is resolved first and
/// only the last component is opened with NOFOLLOW: intermediate symlinks
/// (e.g. `/var` → `/private/var`) stay allowed, a symlinked file does not.
fn open_checked(db_path: &Path) -> Result<rusqlite::Connection, &'static str> {
    let (Some(parent), Some(name)) = (db_path.parent(), db_path.file_name()) else {
        return Err("login_unreadable");
    };
    let path = std::fs::canonicalize(parent)
        .map_err(|_| "login_unreadable")?
        .join(name);
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let conn =
        rusqlite::Connection::open_with_flags(path, flags).map_err(|_| "login_unreadable")?;
    conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
        .map_err(|_| "login_unreadable")?;
    conn.pragma_update(None, "trusted_schema", false)
        .map_err(|_| "login_unreadable")?;
    Ok(conn)
}

#[cfg(unix)]
pub(crate) fn current_uid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: getuid(2) takes no arguments, cannot fail and touches no memory.
    unsafe { getuid() }
}

fn query_login(conn: &rusqlite::Connection) -> rusqlite::Result<Option<CursorLogin>> {
    conn.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
    let mut stmt = conn.prepare(
        "SELECT key, value FROM ItemTable WHERE key IN \
         ('cursorAuth/accessToken','glass.lastSignedInAuthId','cursorAuth/cachedScopedProfile')",
    )?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;

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
    // Every `user_` occurrence, not only the first: a key such as `"user_id"`
    // before the real id must not hide it (it feeds the S-1 account check).
    text.match_indices("user_").find_map(|(start, _)| {
        let rest = &text[start + "user_".len()..];
        let len = rest
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_alphanumeric())
            .map(|(i, c)| i + c.len_utf8())
            .last()
            .unwrap_or(0);
        (len >= 20).then(|| format!("user_{}", &rest[..len]))
    })
}

#[cfg(test)]
mod extract_user_id_tests {
    use super::extract_user_id;

    #[test]
    fn a_short_user_prefix_before_the_real_id_does_not_hide_it() {
        let id = "user_01ABCDEFGHIJKLMNOPQRSTUV";
        assert_eq!(
            extract_user_id(&format!(r#"{{"user_id":"x","sub":"{id}"}}"#)).as_deref(),
            Some(id)
        );
        // Control: no qualifying occurrence at all.
        assert_eq!(extract_user_id(r#"{"user_id":"user_short"}"#), None);
    }
}

#[cfg(test)]
mod checked_open_tests {
    use super::*;

    fn db(dir: &Path) -> PathBuf {
        let path = dir.join("state.vscdb");
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)")
            .unwrap();
        path
    }

    #[test]
    fn the_sync_reader_runs_untrusted_schema_and_defensive() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = open_checked(&db(tmp.path())).unwrap();
        let trusted: i64 = conn
            .query_row("PRAGMA trusted_schema", [], |row| row.get(0))
            .unwrap();
        assert_eq!(trusted, 0);
        assert!(conn
            .db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE)
            .unwrap());
    }

    /// The open itself refuses a symlinked file (defence behind the
    /// `symlink_metadata` check), while a symlinked directory is fine.
    #[cfg(unix)]
    #[test]
    fn the_open_refuses_a_symlinked_file_but_not_a_symlinked_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let real = db(tmp.path());
        let link = tmp.path().join("link.vscdb");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(open_checked(&link).is_err());
        let dir_link = tmp.path().join("dir-link");
        std::os::unix::fs::symlink(tmp.path(), &dir_link).unwrap();
        assert!(open_checked(&dir_link.join("state.vscdb")).is_ok());
        assert_eq!(read_login_checked(&link).err(), Some("db_symlink"));
    }

    /// 7: a database owned by another user is refused before it is opened.
    #[cfg(unix)]
    #[test]
    fn a_foreign_owned_database_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = db(tmp.path());
        let me = current_uid();
        assert_eq!(
            read_login_checked_for(&path, me.wrapping_add(1)).err(),
            Some("db_foreign_owner")
        );
        // Control: the owner's own file opens (no token: `None`).
        assert!(matches!(read_login_checked_for(&path, me), Ok(None)));
    }
}
