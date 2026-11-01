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
//! path from the profile root, are read with `read_login_checked` (a
//! symlinked database is refused), and never feed Grok Bot. The default
//! profile's sync read follows a symlink and opens the target the checked
//! way, without an owner check (`read_login_resolved`).

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
/// default profile's sync read is `read_login_resolved`; Grok Bot keeps
/// `read_login`.
pub(crate) fn read_login_checked(db_path: &Path) -> Result<Option<CursorLogin>, &'static str> {
    #[cfg(unix)]
    let owner = Some(current_uid());
    #[cfg(not(unix))]
    let owner = None;
    read_checked(db_path, owner)
}

/// The default profile's reader for the sync: follows a symlinked
/// `state.vscdb` (including one named by `TOKENBAR_CURSOR_STATE_VSCDB`), as
/// every release did, then opens the resolved file the checked way (regular
/// file, NOFOLLOW, DEFENSIVE, trusted_schema=OFF) without an owner check
/// (earlier releases had none; volumes with ignored ownership report uid
/// 99). A path that cannot be resolved or is not a regular file is "not
/// signed in", as `read_login` treats it. Grok Bot keeps `read_login`.
pub(crate) fn read_login_resolved(db_path: &Path) -> Result<Option<CursorLogin>, &'static str> {
    let Ok(real) = std::fs::canonicalize(db_path) else {
        return Ok(None);
    };
    match read_checked(&real, None) {
        Err("db_not_file") => Ok(None),
        other => other,
    }
}

/// `owner`: the uid the file must belong to; `None` skips that check.
fn read_checked(db_path: &Path, owner: Option<u32>) -> Result<Option<CursorLogin>, &'static str> {
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
        if owner.is_some_and(|uid| meta.uid() != uid) {
            return Err("db_foreign_owner");
        }
    }
    #[cfg(not(unix))]
    let _ = owner;
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

    /// The default profile follows a symlinked database and opens the
    /// target; a target that is not a regular file, an absent one, or one
    /// below a non-directory is "not signed in", as `read_login` treats it.
    #[cfg(unix)]
    #[test]
    fn the_default_reader_follows_a_symlink_and_treats_non_files_as_signed_out() {
        let tmp = tempfile::tempdir().unwrap();
        let path = db(tmp.path());
        let link = tmp.path().join("link.vscdb");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(matches!(read_login_resolved(&link), Ok(None)));
        let to_dir = tmp.path().join("dir-link.vscdb");
        std::os::unix::fs::symlink(tmp.path(), &to_dir).unwrap();
        for signed_out in [
            to_dir,
            tmp.path().to_path_buf(),
            tmp.path().join("absent.vscdb"),
            path.join("below-a-file.vscdb"),
        ] {
            assert!(
                matches!(read_login_resolved(&signed_out), Ok(None)),
                "{signed_out:?}"
            );
        }
    }

    /// Owner check: added profiles refuse another user's database; the
    /// default profile does not check the owner (earlier releases had no such
    /// check, and volumes with ignored ownership report uid 99). A root-owned
    /// regular file that is not a database shows which check ran.
    #[cfg(unix)]
    #[test]
    fn only_added_profiles_check_the_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let path = db(tmp.path());
        let me = current_uid();
        assert_eq!(
            read_checked(&path, Some(me.wrapping_add(1))).err(),
            Some("db_foreign_owner")
        );
        // Control: the owner's own file opens (no token: `None`).
        assert!(matches!(read_checked(&path, Some(me)), Ok(None)));
        let foreign = Path::new("/etc/hosts");
        if me != 0 && foreign.is_file() {
            assert_eq!(read_login_checked(foreign).err(), Some("db_foreign_owner"));
            assert_eq!(read_login_resolved(foreign).err(), Some("login_unreadable"));
        }
    }
}
