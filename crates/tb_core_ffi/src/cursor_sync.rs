//! Cursor usage synced from the signed-in Cursor desktop app.
//!
//! Data flow, per Cursor profile (the default one plus each added
//! `--user-data-dir` root, walked one at a time): its `state.vscdb`
//! (read-only; `cursor_desktop::read_login_checked` for an added profile,
//! `read_login_resolved` for the default) → access token + its
//! own JWT `sub` → POST `get-filtered-usage-events` on cursor.com, full
//! history, paginated → a trimmed JSON file `usage.<hmac>.json` in that
//! profile's Syrtis-owned dir (`<appDir>/cursor-cache` for the default,
//! `<appDir>/cursor-profiles/<uuid>` for an added one) → the engine scans
//! the newest copy of each account's file as an extra Cursor root
//! (`apply_takeover`, files only, never a directory). Aggregation happens in
//! the engine; this module only produces the files and the scan settings.
//!
//! Security contract (plan `cursor-desktop-sync-plan.md`, S-1..S-6, P3-*):
//! - The token is never logged, stored, or put in a status/error string;
//!   every failure maps to a fixed literal `reason`.
//! - The cookie is keyed by the sent token's own JWT `sub`; a stored glass or
//!   profile id that differs refuses the walk (S-1). Token, cookie and target
//!   are fixed for one walk.
//! - An expired token is refused locally, before any request; Cursor's
//!   refresh token is never used (rotating it could sign the user out).
//! - The endpoint is a constant; only tests inject one, by parameter (S-4).
//! - Redirects are never followed and production is https-only; any 3xx
//!   (cursor.com sends an expired session to WorkOS) means `expired`.
//! - The switch is in Rust, default off, and is re-read before every page (S-3).
//! - Only the engine parser's fields are deserialised and written (S-6).
//! - Only a complete walk replaces the file; partial or failed walks leave it.
//! - Every round re-checks the appDir first; one that no longer passes
//!   (`check_app_dir`) gets no read, lock, write or takeover file
//!   (`app_dir_rejected`).
//! - Swift passes a bundle id and `(uuid, root)` pairs, never a directory or a
//!   database path; Rust derives both (CMP-520 B2). `sub` never crosses the
//!   FFI: a profile whose account an earlier profile already covered this
//!   round reports only that profile's id (`duplicateOf`). A walk covers its
//!   account when it synced, or failed in a way a second session would only
//!   repeat (`covers_account`): every failure except one specific to that
//!   profile's session or to this machine — expired (local expiry, 401,
//!   redirect, 403 JSON), disabled (removed mid-walk), client_unavailable,
//!   write_failed — which leaves the account to the next profile on it.

use crate::cursor_desktop::{self, CursorLogin};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const USAGE_EVENTS_URL: &str = "https://cursor.com/api/dashboard/get-filtered-usage-events";
const FILE_NAME_DOMAIN: &str = "cursor-sync-file";
const LOCK_FILE_NAME: &str = ".cursor-sync.lock";
/// Starts with a dot so it can never match the engine's `usage*` patterns: a
/// half-written temp file must not be scanned.
const TEMP_FILE_PREFIX: &str = ".cursor-sync.tmp-";

const PAGE_SIZE: u32 = 500;
/// Upstream's per-page timeout (`CURSOR_HTTP_TIMEOUT`), clamped to the budget.
const PAGE_TIMEOUT: Duration = Duration::from_secs(15);
/// 256 pages × 500 = 128k events. The byte cap below normally binds first.
const MAX_PAGES: u32 = 256;
/// Cumulative over the walk. C0.5 measured ≈ 553 bytes/event (33173 bytes for
/// 60 events), so 64 MiB is ≈ 120k events, far above the largest history
/// seen (3542 events ≈ 2 MB).
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
/// Only a 403 body is read, to tell an auth refusal (JSON) from an HTML wall.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
/// D4: the first sync (no complete file yet) and every explicit "Sync now".
const FULL_BUDGET: Duration = Duration::from_secs(10 * 60);
/// D4: later background syncs, ≥ 2× the measured full walk and ≤ 10 min.
/// C0.5: the largest known history, 3542 events ≈ 8 pages at ~1 s/page,
/// walks in ≈ 10 s; 60 s is 6× that. A walk that does not fit leaves the
/// existing complete file in place and reports `partial`.
const BACKGROUND_BUDGET: Duration = Duration::from_secs(60);
/// A token this close to `exp` could expire mid-walk; treat it as expired.
const EXP_SKEW_SECS: f64 = 60.0;

// ---------------------------------------------------------------------------
// Switch registry (in-memory, default off; Swift re-applies it at launch).

/// The default Cursor profile's sync dir, `<appDir>/cursor-cache` (the one
/// location every release has used, so no migration).
const DEFAULT_DIR_NAME: &str = "cursor-cache";
/// Added profiles live in `<appDir>/cursor-profiles/<uuid>/`, a sibling of
/// `cursor-cache` and never nested in it: a build that registers the
/// `cursor-cache` dir (before CMP-520) never walks into them.
const PROFILES_DIR_NAME: &str = "cursor-profiles";
/// The default profile's id in statuses and results; not a UUID, so it can
/// never collide with an added profile's.
const DEFAULT_PROFILE_ID: &str = "default";
/// `cleanupFailedProfiles` entry: a previous appDir that no longer passes
/// `check_app_dir` (nothing in it was cleaned) or whose files could not all be
/// deleted. It is remembered in `STALE_APP_DIRS` and retried by every accepted
/// push while Syrtis keeps running; after a relaunch it is no longer known.
const APP_DIR_FAILURE: &str = "appDir";
/// Bounds the per-sync work one config can ask for.
const MAX_PROFILES: usize = 32;

/// An added Cursor profile (`--user-data-dir`): a Swift-assigned UUID and the
/// folder the user chose. Never an account id or anything derived from one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Profile {
    pub id: String,
    pub root: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Config {
    pub enabled: bool,
    /// `<home>/Library/Application Support/<bundle id>`, derived here from
    /// the validated bundle id; Swift never passes a directory (B2).
    pub app_dir: Option<PathBuf>,
    /// Added profiles only; the default profile is implicit.
    pub profiles: Vec<Profile>,
    pub cli_takeover_confirmed: bool,
}

static CONFIG: LazyLock<RwLock<Config>> = LazyLock::new(|| RwLock::new(Config::default()));

/// Previous appDirs whose files could not be cleaned (refused by
/// `check_app_dir` now, or a delete failed); every accepted push retries them
/// until they are clean, while Syrtis keeps running. Not persisted: after a
/// relaunch the previous appDir is no longer known and is not retried.
static STALE_APP_DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

pub(crate) fn config() -> Config {
    #[cfg(test)]
    if let Some(config) = THREAD_CONFIG.with(|c| c.borrow().clone()) {
        return config;
    }
    CONFIG
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn default_dir(app_dir: &Path) -> PathBuf {
    app_dir.join(DEFAULT_DIR_NAME)
}

fn profiles_dir(app_dir: &Path) -> PathBuf {
    app_dir.join(PROFILES_DIR_NAME)
}

/// One profile to sync: where its usage file goes and which login it reads.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Slot {
    id: String,
    /// None for the default profile.
    root: Option<PathBuf>,
    dir: PathBuf,
    db: PathBuf,
}

/// The default profile first, then the added ones in config order. The
/// default reads `default_db` (the fixed path, or `TOKENBAR_CURSOR_STATE_VSCDB`);
/// an added one only ever reads `<root>/User/globalStorage/state.vscdb`.
fn slots(config: &Config, default_db: &Path) -> Vec<Slot> {
    let Some(app_dir) = config.app_dir.as_deref() else {
        return Vec::new();
    };
    let mut slots = vec![Slot {
        id: DEFAULT_PROFILE_ID.to_string(),
        root: None,
        dir: default_dir(app_dir),
        db: default_db.to_path_buf(),
    }];
    slots.extend(config.profiles.iter().map(|p| Slot {
        id: p.id.clone(),
        root: Some(p.root.clone()),
        dir: profiles_dir(app_dir).join(&p.id),
        db: p.root.join("User/globalStorage/state.vscdb"),
    }));
    slots
}

impl Slot {
    /// Still configured exactly as when the walk started: sync on, same
    /// appDir, and (for an added profile) the same `(id, root)`.
    fn still_in(&self, start: &Config, now: &Config) -> bool {
        now.enabled
            && now.app_dir == start.app_dir
            && self.root.as_ref().is_none_or(|root| {
                now.profiles
                    .iter()
                    .any(|p| p.id == self.id && &p.root == root)
            })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SetInput {
    enabled: bool,
    bundle_id: String,
    #[serde(default)]
    profiles: Vec<ProfileInput>,
    #[serde(default)]
    cli_takeover_confirmed: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileInput {
    id: String,
    root: String,
}

/// `<HOME>/.config/tokscale/cursor-cache`, the CLI's Cursor root
/// (engine `clients.rs`, `PathRoot::Home`).
fn cli_root(home: &Path) -> PathBuf {
    home.join(".config/tokscale/cursor-cache")
}

/// Reverse-DNS: `[A-Za-z0-9-]` labels, at least two, none empty (so no
/// `..`, no `/`, no leading or trailing dot).
fn valid_bundle_id(id: &str) -> bool {
    id.len() <= 255
        && id.split('.').count() >= 2
        && id.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// Swift's `UUID().uuidString`: 8-4-4-4-12 uppercase hex. Uppercase only, so
/// two ids can never name the same dir on a case-insensitive volume.
fn valid_profile_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'A'..=b'F').contains(&b),
        })
}

/// `path` with its longest existing ancestor resolved, the rest appended.
fn resolved(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut cursor = path;
    loop {
        if let Ok(real) = std::fs::canonicalize(cursor) {
            return rest.iter().rev().fold(real, |acc, part| acc.join(part));
        }
        match (cursor.parent(), cursor.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                cursor = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

fn app_dir_for(bundle_id: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    if !valid_bundle_id(bundle_id) {
        return Err("cursor sync bundle id is invalid".to_string());
    }
    let home = home.ok_or_else(|| "cursor sync needs a home directory".to_string())?;
    let app_dir = home.join("Library/Application Support").join(bundle_id);
    check_app_dir(&app_dir, home)?;
    Ok(app_dir)
}

/// An appDir Syrtis may write into or clean: absent, or (followed through a
/// symlink, as earlier releases allowed) a directory owned by this user;
/// never inside the tokscale CLI's tree. Re-run on an old appDir before
/// cleaning it, since it may have changed since it was accepted.
fn check_app_dir(app_dir: &Path, home: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(app_dir) {
        Ok(_) if own_real_dir(app_dir) => {}
        Ok(_) => return Err("cursor sync app dir is not a directory of this user".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("cursor sync app dir cannot be examined".to_string()),
    }
    // D1: Syrtis never writes into the tokscale CLI's tree (and a sync file
    // there would be dropped by the takeover's own exclusion), including
    // through a symlinked ancestor.
    let tokscale = home.join(".config/tokscale");
    if app_dir.starts_with(&tokscale) || resolved(app_dir).starts_with(resolved(&tokscale)) {
        return Err("cursor sync app dir must not be inside ~/.config/tokscale".to_string());
    }
    Ok(())
}

/// `path`, followed through symlinks, is a directory owned by this user.
fn own_real_dir(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if meta.uid() != cursor_desktop::current_uid() {
            return false;
        }
    }
    meta.is_dir()
}

fn validate_root(raw: &str) -> Result<PathBuf, String> {
    let root = PathBuf::from(raw);
    if !root.is_absolute()
        || root.parent().is_none()
        || root.components().any(|c| matches!(c, Component::ParentDir))
    {
        return Err("cursor profile root must be an absolute path without '..'".to_string());
    }
    Ok(root)
}

/// Cursor's own user-data folder, the default profile's.
fn cursor_own_root(home: &Path) -> PathBuf {
    home.join("Library/Application Support/Cursor")
}

/// The folder's file identity (device and inode, a final symlink followed),
/// so another spelling of one folder (a symlink, a firmlink, another case on
/// a case-insensitive volume) compares equal. `None` for a folder that
/// cannot be read (gone, unmounted): never an identity match, so it can only
/// match by path.
fn folder_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::metadata(path).ok()?;
    meta.is_dir().then(|| (meta.dev(), meta.ino()))
}

/// Whether `a` and `b` name one folder: the same path, or the same
/// directory identity.
fn same_folder(a: &Path, b: &Path) -> bool {
    a == b || folder_identity(a).is_some_and(|identity| Some(identity) == folder_identity(b))
}

/// An off push is always accepted: it turns sync (and so the takeover) off
/// whatever else it carries. Only its bundle id is read; an appDir that
/// fails `app_dir_for` becomes `None`, and nothing in it is touched.
fn parse_off(raw: &str, home: Option<&Path>) -> Option<Config> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    if value.get("enabled") != Some(&serde_json::Value::Bool(false)) {
        return None;
    }
    let bundle_id = value
        .get("bundleId")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    Some(Config {
        enabled: false,
        app_dir: app_dir_for(bundle_id, home).ok(),
        profiles: Vec::new(),
        cli_takeover_confirmed: value
            .get("cliTakeoverConfirmed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

/// The registry from an on push, plus the profiles refused for their root
/// (`{"index","reason"}`, index in the pushed list; reasons `cursor_default`
/// and `duplicate`). Whole-push errors stay for what makes the list itself
/// unusable (bad JSON, bundle id, too many profiles, an invalid or repeated
/// id, a root that is not an absolute path): a refused root drops only its own
/// profile, so the default profile and the other profiles keep syncing.
fn parse_config(
    raw: &str,
    home: Option<&Path>,
) -> Result<(Config, Vec<serde_json::Value>), String> {
    let input: SetInput =
        serde_json::from_str(raw).map_err(|_| "invalid cursor sync JSON".to_string())?;
    let app_dir = app_dir_for(&input.bundle_id, home)?;
    if input.profiles.len() > MAX_PROFILES {
        return Err("too many cursor profiles".to_string());
    }
    let mut profiles: Vec<Profile> = Vec::with_capacity(input.profiles.len());
    let mut ids: Vec<String> = Vec::with_capacity(input.profiles.len());
    let mut rejected = Vec::new();
    let own = home.map(cursor_own_root);
    for (index, profile) in input.profiles.into_iter().enumerate() {
        if !valid_profile_id(&profile.id) || ids.contains(&profile.id) {
            return Err("cursor profile id is invalid".to_string());
        }
        ids.push(profile.id.clone());
        let root = validate_root(&profile.root)?;
        // The Swift check at add time is a pre-check of these two rules, which
        // the core owns: Cursor's own folder is the default profile (a second
        // read of the same login), and a folder already listed (an earlier
        // entry wins) would be a second profile on the same database.
        let reason = if own.as_deref().is_some_and(|own| same_folder(&root, own)) {
            Some("cursor_default")
        } else if profiles.iter().any(|p| same_folder(&root, &p.root)) {
            Some("duplicate")
        } else {
            None
        };
        match reason {
            Some(reason) => rejected.push(serde_json::json!({"index": index, "reason": reason})),
            None => profiles.push(Profile {
                id: profile.id,
                root,
            }),
        }
    }
    Ok((
        Config {
            enabled: input.enabled,
            app_dir: Some(app_dir),
            profiles,
            cli_takeover_confirmed: input.cli_takeover_confirmed,
        },
        rejected,
    ))
}

/// Replace the registry from
/// `{"enabled":bool,"bundleId":"a.b","profiles":[{"id":"<UUID>","root":"/abs"}],"cliTakeoverConfirmed":bool}`.
/// Invalid input leaves the registry untouched; a profile whose root is
/// Cursor's own folder or repeats an earlier one is left out and reported in
/// `rejectedProfiles` by index (`parse_config`). Cleanup (P3-7, B2): turning
/// sync off deletes our files from every profile dir; while on, dirs of
/// profiles no longer listed (orphans) are emptied of our files and removed.
pub(crate) fn set_from_json(raw: &str, home: Option<&Path>) -> Result<serde_json::Value, String> {
    let (new, rejected_profiles) = match parse_off(raw, home) {
        Some(off) => (off, Vec::new()),
        None => parse_config(raw, home)?,
    };
    let old = {
        let mut guard = CONFIG
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::replace(&mut *guard, new.clone())
    };
    let mut cleanup = Cleanup::default();
    // The appDir in use until now (files may be left there) is cleaned only
    // if it still passes the checks; otherwise it is reported. A refused new
    // appDir with no earlier one has nothing of ours: no failure. A previous
    // appDir that could not be cleaned is kept in `STALE_APP_DIRS` and tried
    // again by every accepted push until it is clean, so a later push with
    // the same appDir cannot hide the failure (in this process only).
    {
        let mut stale = STALE_APP_DIRS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(old_app) = old.app_dir.as_deref() {
            if new.app_dir.as_deref() != Some(old_app) && !stale.iter().any(|d| d == old_app) {
                stale.push(old_app.to_path_buf());
            }
        }
        // An appDir that is the current one again is cleaned by the passes below.
        stale.retain(|dir| new.app_dir.as_deref() != Some(dir.as_path()));
        stale.retain(|dir| {
            let mut attempt = Cleanup::default();
            let clean = match home.map(|home| check_app_dir(dir, home)) {
                Some(Ok(())) => {
                    attempt.everything(dir);
                    attempt.failed.is_empty()
                }
                _ => false,
            };
            cleanup.removed += attempt.removed;
            if !clean && !cleanup.failed.iter().any(|id| id == APP_DIR_FAILURE) {
                cleanup.failed.push(APP_DIR_FAILURE.to_string());
            }
            !clean
        });
    }
    match (new.app_dir.as_deref(), new.enabled) {
        (Some(app_dir), true) => cleanup.orphans(app_dir, &new.profiles),
        (Some(app_dir), false) => cleanup.everything(app_dir),
        (None, _) => {}
    }
    let profiles: Vec<_> = slots(&new, Path::new(""))
        .into_iter()
        .map(|slot| serde_json::json!({"id": slot.id, "dir": slot.dir.to_string_lossy()}))
        .collect();
    Ok(serde_json::json!({
        "enabled": new.enabled,
        "appDir": new.app_dir.as_deref().map(Path::to_string_lossy),
        "profiles": profiles,
        "cliTakeoverConfirmed": new.cli_takeover_confirmed,
        "removedFiles": cleanup.removed,
        "cleanupFailed": !cleanup.failed.is_empty(),
        "cleanupFailedProfiles": cleanup.failed,
        "rejectedProfiles": rejected_profiles,
    }))
}

#[derive(Default)]
struct Cleanup {
    removed: usize,
    /// What could not be fully cleaned: a profile id (`default` or a UUID)
    /// whose files could not all be deleted, `cursor-profiles` (that parent
    /// folder could not be read), or `appDir` (a previous appDir was refused
    /// or its files could not all be deleted; see `STALE_APP_DIRS`).
    failed: Vec<String>,
}

impl Cleanup {
    fn record(&mut self, id: &str, (removed, failed): (usize, bool)) {
        self.removed += removed;
        if failed {
            self.failed.push(id.to_string());
        }
    }

    fn everything(&mut self, app_dir: &Path) {
        self.record(
            DEFAULT_PROFILE_ID,
            remove_usage_files_locked(&default_dir(app_dir), false),
        );
        self.orphans(app_dir, &[]);
    }

    /// Every `<uuid>` dir under `cursor-profiles` not in `keep`. Only real
    /// directories with a UUID name are touched: a symlink is not followed
    /// (`remove_usage_files_locked`) and anything else is left alone.
    fn orphans(&mut self, app_dir: &Path, keep: &[Profile]) {
        let parent = profiles_dir(app_dir);
        match std::fs::symlink_metadata(&parent) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => return self.failed.push(PROFILES_DIR_NAME.to_string()),
        }
        let Ok(entries) = std::fs::read_dir(&parent) else {
            return self.failed.push(PROFILES_DIR_NAME.to_string());
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str().filter(|n| valid_profile_id(n)) else {
                continue;
            };
            if keep.iter().any(|p| p.id == name) {
                continue;
            }
            // Skips (as nothing of ours) an entry that is not a real dir.
            self.record(name, remove_usage_files_locked(&entry.path(), true));
        }
    }
}

// ---------------------------------------------------------------------------
// Takeover (Core rule + D6 ownership guard).

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Takeover {
    Off,
    /// CLI Cursor files exist and the user has not confirmed: `cliPresent`.
    Blocked,
    On {
        cli_root: PathBuf,
        /// One file per account (B1): never a directory.
        files: Vec<PathBuf>,
    },
}

pub(crate) fn takeover(config: &Config, home: Option<&Path>) -> Takeover {
    let (true, Some(home)) = (config.enabled, home) else {
        return Takeover::Off;
    };
    // Never register files from an appDir that no longer passes the checks.
    if config
        .app_dir
        .as_deref()
        .is_some_and(|app_dir| check_app_dir(app_dir, home).is_err())
    {
        return Takeover::Off;
    }
    let files = newest_per_account(config);
    if files.is_empty() {
        return Takeover::Off;
    }
    let cli_root = cli_root(home);
    if cli_has_cursor_files(&cli_root) && !config.cli_takeover_confirmed {
        return Takeover::Blocked;
    }
    Takeover::On { cli_root, files }
}

/// B1: every complete sync file across the default and the added profile
/// dirs, grouped by file name (`usage.<HMAC(sub)>.json`, one name per
/// account), keeping the newest copy of each. A stale copy in another
/// profile's dir (an account switch whose delete failed) is never returned.
fn newest_per_account(config: &Config) -> Vec<PathBuf> {
    let mut newest: std::collections::BTreeMap<String, (SystemTime, PathBuf)> =
        std::collections::BTreeMap::new();
    for slot in slots(config, Path::new("")) {
        for (path, modified) in sync_files(&slot.dir) {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            // Strictly newer: on a tie the earlier profile (default first) wins.
            if newest.get(&name).is_none_or(|(seen, _)| modified > *seen) {
                newest.insert(name, (modified, path));
            }
        }
    }
    newest
        .into_values()
        .map(|(_, path)| std::fs::canonicalize(&path).unwrap_or(path))
        .collect()
}

/// Merge the takeover into settings already holding the registry's extra
/// paths (e.g. Claude's), never replacing them. Only file paths are
/// registered, never `cursor-cache`, `cursor-profiles` or a profile dir (B3);
/// the engine walks a file root as that single file.
pub(crate) fn apply_takeover(
    settings: &mut tokscale_core::scanner::ScannerSettings,
    config: &Config,
    home: Option<&Path>,
) {
    if let Takeover::On { cli_root, files } = takeover(config, home) {
        settings
            .extra_scan_paths
            .entry("cursor".to_string())
            .or_default()
            .extend(files);
        settings
            .excluded_scan_paths
            .entry("cursor".to_string())
            .or_default()
            .push(cli_root);
    }
}

/// Any `usage*.csv` / `usage*.json` anywhere under the CLI root, including
/// files the engine would skip (archive, backups): the ownership guard errs
/// toward asking. An unreadable root counts as present for the same reason.
fn cli_has_cursor_files(root: &Path) -> bool {
    fn walk(dir: &Path, depth: u32) -> bool {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => return error.kind() != std::io::ErrorKind::NotFound,
        };
        entries.flatten().any(|entry| {
            let Ok(kind) = entry.file_type() else {
                return true;
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if kind.is_dir() {
                return depth < 4 && walk(&entry.path(), depth + 1);
            }
            name.starts_with("usage") && (name.ends_with(".csv") || name.ends_with(".json"))
        })
    }
    walk(root, 0)
}

fn is_sync_file_name(name: &str) -> bool {
    name.strip_prefix("usage.")
        .and_then(|rest| rest.strip_suffix(".json"))
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Every complete synced file in `dir` (a real directory), with its mtime.
/// Only complete walks ever create one, so existence is completeness.
fn sync_files(dir: &Path) -> Vec<(PathBuf, SystemTime)> {
    if !std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| is_sync_file_name(&entry.file_name().to_string_lossy()))
        .filter_map(|entry| {
            let meta = std::fs::symlink_metadata(entry.path()).ok()?;
            let modified = meta.modified().ok()?;
            meta.is_file().then(|| (entry.path(), modified))
        })
        .collect()
}

/// The newest complete synced file in `dir`, with its mtime.
fn complete_file(dir: &Path) -> Option<(PathBuf, SystemTime)> {
    sync_files(dir)
        .into_iter()
        .max_by_key(|(_, modified)| *modified)
}

// ---------------------------------------------------------------------------
// Files.

/// What a delete pass did: `removed` counts every file of ours (usage files
/// and temp leftovers), `usage` only the usage files, which are what the
/// engine reads (a temp file never changes what is counted).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Removal {
    removed: usize,
    usage: usize,
    failed: bool,
}

/// Delete Syrtis usage files (and temp leftovers) in `dir` except `keep`.
/// Never touches anything else. Caller holds the dir lock. `failed`: any of
/// ours could not be deleted, or the dir could not be read.
fn remove_usage_files(dir: &Path, keep: Option<&str>) -> Removal {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Removal {
            failed: true,
            ..Removal::default()
        };
    };
    let mut result = Removal::default();
    for entry in entries {
        let Ok(entry) = entry else {
            result.failed = true;
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !is_ours(&name) || Some(name.as_ref()) == keep {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => {
                result.removed += 1;
                result.usage += usize::from(is_sync_file_name(&name));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => result.failed = true,
        }
    }
    result
}

/// B2: exactly our file names, `usage.<64 hex>.json` and our temp prefix.
fn is_ours(name: &str) -> bool {
    is_sync_file_name(name) || name.starts_with(TEMP_FILE_PREFIX)
}

/// `(removed, failed)`. A missing dir, or one that is not a real directory,
/// holds nothing of ours: nothing to delete, not a failure. A dir that cannot
/// be examined, or whose files of ours cannot be deleted, is a failure.
/// `remove_dir`: an orphaned profile dir; once our files are gone, drop our
/// lock file and `rmdir` it, which succeeds only if nothing else is left
/// (never `remove_dir_all`).
fn remove_usage_files_locked(dir: &Path, remove_dir: bool) -> (usize, bool) {
    let result = locked_removal(dir, None, remove_dir);
    (result.removed, result.failed)
}

/// `remove_usage_files_locked` that spares `keep` (a usage file name) and
/// reports the usage files separately.
fn locked_removal(dir: &Path, keep: Option<&str>, remove_dir: bool) -> Removal {
    let nothing = Removal::default();
    // Only a real directory: never follow a symlink to delete elsewhere.
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return nothing,
        Err(error) => {
            return Removal {
                failed: error.kind() != std::io::ErrorKind::NotFound,
                ..nothing
            }
        }
    }
    let locked = crate::agent_quota_history::open_owner_only(&dir.join(LOCK_FILE_NAME))
        .ok()
        .filter(|lock| lock.lock_exclusive().is_ok());
    if locked.is_none() {
        // No lock, no delete; a failure only if something of ours is there.
        // An entry that cannot be read counts as ours (fail closed).
        let holds_ours = std::fs::read_dir(dir).map_or(true, |mut entries| {
            entries.any(|entry| {
                entry.map_or(true, |entry| is_ours(&entry.file_name().to_string_lossy()))
            })
        });
        return Removal {
            failed: holds_ours,
            ..nothing
        };
    }
    let result = remove_usage_files(dir, keep);
    if remove_dir && !result.failed {
        let _ = std::fs::remove_file(dir.join(LOCK_FILE_NAME));
        let _ = std::fs::remove_dir(dir);
    }
    result
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_file_name() -> String {
    format!(
        "{TEMP_FILE_PREFIX}{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageFile<'a> {
    total_usage_events_count: usize,
    usage_events_display: &'a [Event],
}

/// Whether the usage file at `path` already holds exactly `bytes`; if so its
/// modified time is set to now, on the same handle, so it still dates the
/// last complete walk (`lastSuccessMs`, the newest copy per account) without
/// a rewrite. Any failure answers `false`: the caller then writes as usual.
/// A symlink or a non-regular file is never read (it is replaced instead);
/// the length is compared first, so a file of another size is not read.
fn refresh_if_same(path: &Path, bytes: &[u8]) -> bool {
    use std::io::Read as _;
    (|| -> std::io::Result<bool> {
        let meta = std::fs::symlink_metadata(path)?;
        if !meta.is_file() || meta.len() != bytes.len() as u64 {
            return Ok(false);
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        if file.metadata()?.len() != bytes.len() as u64 {
            return Ok(false);
        }
        let mut old = Vec::with_capacity(bytes.len());
        file.read_to_end(&mut old)?;
        if old != bytes {
            return Ok(false);
        }
        file.set_modified(SystemTime::now())?;
        Ok(true)
    })()
    .unwrap_or(false)
}

/// Write the complete walk: dir 0700, temp 0600, fsync, atomic rename, dir
/// fsync, then drop every other usage file. Returns whether a usage file's
/// content changed: a file whose bytes already equal the new ones is not
/// rewritten (only its modified time moves to now), so an identical walk
/// changes nothing, unless a superseded usage file was removed (an orphan
/// temp does not count).
///
/// `still_wanted` is checked before anything is created, so a profile or
/// switch turned off earlier (say, during its last page) creates no dir and
/// no lock file. That check is not atomic with the creation: a removal racing
/// it can still leave an empty dir and lock file. It is checked again under
/// the lock, which is what guarantees that no usage file is written after a
/// concurrent disable or removal, because those delete under the same lock.
fn commit_file(
    dir: &Path,
    stem: &str,
    events: &[Event],
    still_wanted: &dyn Fn() -> bool,
) -> Result<Committed, Stop> {
    let failed = || Stop::error("write_failed");
    if !still_wanted() {
        return Err(Stop::new(State::Disabled, None));
    }
    // An added profile's dir sits in `cursor-profiles`: that must be a real
    // directory too, or `create_dir_all` would follow a symlink there.
    if dir.parent().and_then(Path::file_name) == Some(PROFILES_DIR_NAME.as_ref()) {
        let parent = dir.parent().unwrap_or(dir);
        crate::agent_quota_history::ensure_real_directory(parent).map_err(|_| failed())?;
    }
    crate::agent_quota_history::ensure_real_directory(dir).map_err(|_| failed())?;
    let lock = crate::agent_quota_history::open_owner_only(&dir.join(LOCK_FILE_NAME))
        .map_err(|_| failed())?;
    lock.lock_exclusive().map_err(|_| failed())?;
    if !still_wanted() {
        return Err(Stop::new(State::Disabled, None));
    }
    let payload = serde_json::to_vec(&UsageFile {
        total_usage_events_count: events.len(),
        usage_events_display: events,
    })
    .map_err(|_| failed())?;
    let final_name = format!("usage.{stem}.json");
    let same = refresh_if_same(&dir.join(&final_name), &payload);
    if !same {
        let temp_path = dir.join(temp_file_name());
        let written = (|| {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options.open(&temp_path)?;
            file.write_all(&payload)?;
            file.sync_all()?;
            drop(file);
            tokscale_core::fs_atomic::replace_file(&temp_path, &dir.join(&final_name))
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&temp_path);
            return Err(failed());
        }
        // The rename is the commit point; a failed dir fsync cannot undo it.
        let _ = crate::agent_quota_history::sync_directory(dir);
    }
    let removal = remove_usage_files(dir, Some(&final_name));
    Ok(Committed {
        changed: !same || removal.usage > 0,
        cleanup_failed: removal.failed,
    })
}

/// What a commit left: whether a usage file's bytes changed, and whether
/// another usage file of ours (a superseded account's) could not be deleted.
#[derive(Debug)]
struct Committed {
    changed: bool,
    cleanup_failed: bool,
}

// ---------------------------------------------------------------------------
// Wire types: exactly the fields the engine's Cursor JSON parser reads
// (tokscale-core `sessions/cursor.rs`, `CursorUsageEvent` /
// `CursorTokenUsage`, pin 8fc63ced), S-6. Everything else — owningUser,
// serviceAccountId, subscriptionProductId, customSubscriptionName, kind,
// cost breakdowns, flags — is dropped by construction.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    usage_events_display: Vec<Event>,
    #[serde(default)]
    total_usage_events_count: Option<u64>,
}

/// The event timestamp: Unix ms as a string (observed) or a number; the
/// engine accepts both.
#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum Timestamp {
    Text(String),
    Number(serde_json::Number),
}

/// A count or cents value. The engine coerces a number or a numeric string
/// (`de_opt_*_lenient`), so both are accepted here; anything else (an object,
/// a non-numeric string) fails the page rather than carrying free text into
/// the file. Written back as a JSON number.
#[derive(Serialize)]
#[serde(transparent)]
struct Num(serde_json::Number);

impl<'de> Deserialize<'de> for Num {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::Number(number) => Ok(Num(number)),
            serde_json::Value::String(text) => text
                .trim()
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Num)
                .ok_or_else(|| D::Error::custom("not a number")),
            _ => Err(D::Error::custom("not a number")),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct TokenUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input_tokens: Option<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_tokens: Option<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_read_tokens: Option<Num>,
    /// Read by the engine (`cache_write`), though absent on many events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_write_tokens: Option<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total_cents: Option<Num>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Event {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timestamp: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    charged_cents: Option<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token_usage: Option<TokenUsage>,
}

// ---------------------------------------------------------------------------
// Outcome.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum State {
    Ok,
    Partial,
    Expired,
    NotSignedIn,
    Offline,
    Error,
    Disabled,
    CliPresent,
    /// The account was already covered this round by an earlier profile.
    Duplicate,
}

/// Every `reason` is a fixed literal: nothing from the token, the account,
/// the response or the filesystem can reach a status string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stop {
    state: State,
    reason: Option<&'static str>,
}

impl Stop {
    fn new(state: State, reason: Option<&'static str>) -> Self {
        Self { state, reason }
    }
    fn error(reason: &'static str) -> Self {
        Self::new(State::Error, Some(reason))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Outcome {
    stop: Stop,
    events: usize,
    /// For `Duplicate`: the index (in this round's slots) of the profile that
    /// already covered the same account. Never the account itself.
    duplicate_of: Option<usize>,
    /// A usage file in this profile's dir was written with new bytes or
    /// removed: the data the engine reads changed.
    changed: bool,
    /// A superseded usage file of ours could not be deleted: by a duplicate's
    /// cleanup, or after a successful commit (reported in
    /// `cleanupFailedProfiles`).
    cleanup_failed: bool,
    /// Stopped by a refusal decided where it arose, so no list of codes can
    /// miss one: every `Err` the login reader returns (all its refusals; no
    /// login is `Ok(None)`), a token that cannot be read or does not match
    /// its account, no installation key, a cookie that cannot be built, an
    /// appDir that fails its checks (so no storage to write to).
    refused: bool,
}

impl Outcome {
    fn stopped(stop: Stop) -> Self {
        Self {
            stop,
            events: 0,
            duplicate_of: None,
            changed: false,
            cleanup_failed: false,
            refused: false,
        }
    }

    fn refused(stop: Stop) -> Self {
        Self {
            refused: true,
            ..Self::stopped(stop)
        }
    }
}

/// Outcomes that write no file however long the round runs: a duplicate, no
/// login, an expired session, or a refusal (`Outcome::refused`). A profile
/// whose last round ended so does not ask for the full budget while it has
/// no file; a timeout, an outage or a spent budget does.
fn writes_no_file(outcome: &Outcome) -> bool {
    outcome.refused
        || matches!(
            outcome.stop.state,
            State::Duplicate | State::NotSignedIn | State::Expired
        )
}

// ---------------------------------------------------------------------------
// The walk.

#[derive(Clone, Copy)]
struct Limits {
    budget: Duration,
    page_timeout: Duration,
    page_size: u32,
    max_pages: u32,
    max_body_bytes: usize,
}

impl Limits {
    fn production(budget: Duration) -> Self {
        Self {
            budget,
            page_timeout: PAGE_TIMEOUT,
            page_size: PAGE_SIZE,
            max_pages: MAX_PAGES,
            max_body_bytes: MAX_BODY_BYTES,
        }
    }
}

/// Everything one sync depends on. Production builds it from constants;
/// tests inject a local server, a fixture database and a fixed key.
struct Target<'a> {
    /// The default profile's database; added profiles derive their own.
    db_path: &'a Path,
    url: &'a str,
    https_only: bool,
    file_stem: &'a dyn Fn(&str) -> Option<String>,
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The cookie user id: the part of the token's own `sub` after the last `|`.
/// Refuses (S-1) when a stored glass/profile id exists and differs.
fn token_user(login: &CursorLogin) -> Result<(String, f64), Stop> {
    let unreadable = Stop::error("token_unreadable");
    if !login
        .access_token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(unreadable);
    }
    let claims = crate::agent_usage::jwt_payload(&login.access_token).ok_or(unreadable)?;
    let sub = claims
        .get("sub")
        .and_then(serde_json::Value::as_str)
        .ok_or(unreadable)?;
    let user = sub.rsplit('|').next().unwrap_or_default();
    if user.is_empty() || !user.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(unreadable);
    }
    for stored in [&login.glass_user_id, &login.profile_user_id]
        .into_iter()
        .flatten()
    {
        if stored != user {
            return Err(Stop::error("account_mismatch"));
        }
    }
    // A missing or non-numeric `exp` cannot be shown to be valid: treat it
    // like an expired login rather than send it.
    let exp = claims
        .get("exp")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    Ok((user.to_string(), exp))
}

enum BodyError {
    TooLarge,
    Timeout,
    Transport,
}

async fn read_capped(response: &mut reqwest::Response, cap: usize) -> Result<Vec<u8>, BodyError> {
    if response
        .content_length()
        .is_some_and(|length| length > cap as u64)
    {
        return Err(BodyError::TooLarge);
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > cap {
                    return Err(BodyError::TooLarge);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(body),
            Err(error) if error.is_timeout() => return Err(BodyError::Timeout),
            Err(_) => return Err(BodyError::Transport),
        }
    }
}

async fn walk(
    target: &Target<'_>,
    cookie: &reqwest::header::HeaderValue,
    limits: Limits,
    enabled: &dyn Fn() -> bool,
) -> Result<Vec<Event>, Stop> {
    use reqwest::header;
    let client = crate::agent_usage::provider_http_client_builder()
        .redirect(reqwest::redirect::Policy::none())
        .https_only(target.https_only)
        .build()
        .map_err(|_| Stop::error("client_unavailable"))?;
    let end_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let deadline = Instant::now() + limits.budget;
    let mut events: Vec<Event> = Vec::new();
    let mut bytes_read = 0usize;
    for page in 1..=limits.max_pages {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Stop::new(State::Partial, Some("budget_exhausted")));
        }
        // S-3: the switch is re-read before every send.
        if !enabled() {
            return Err(Stop::new(State::Disabled, None));
        }
        let body = serde_json::json!({
            "startDate": "0",
            "endDate": end_ms.to_string(),
            "page": page,
            "pageSize": limits.page_size,
        });
        let sent = client
            .post(target.url)
            .header(header::COOKIE, cookie.clone())
            .header(header::ORIGIN, "https://cursor.com")
            .header(header::REFERER, "https://cursor.com/dashboard")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json")
            .header(header::USER_AGENT, "TokenBar")
            .body(body.to_string())
            .timeout(limits.page_timeout.min(remaining))
            .send()
            .await;
        let mut response = match sent {
            Ok(response) => response,
            Err(error) if error.is_timeout() => {
                return Err(Stop::new(State::Partial, Some("timeout")))
            }
            Err(_) => return Err(Stop::new(State::Offline, Some("unreachable"))),
        };
        let status = response.status();
        if status.is_redirection() || status.as_u16() == 401 {
            return Err(Stop::new(State::Expired, None));
        }
        if status.as_u16() == 403 {
            // P3-4: an auth refusal is JSON; an HTML wall (bot check, outage
            // page) is not a sign-in problem and must not tell the user to
            // sign in again.
            let json = read_capped(&mut response, MAX_ERROR_BODY_BYTES)
                .await
                .ok()
                .is_some_and(|b| serde_json::from_slice::<serde_json::Value>(&b).is_ok());
            return Err(if json {
                Stop::new(State::Expired, None)
            } else {
                Stop::error("forbidden_non_json")
            });
        }
        if status.as_u16() == 429 {
            // A maintainer stop condition (plan C2 stops), distinct on purpose.
            return Err(Stop::error("rate_limited"));
        }
        if status.is_server_error() {
            return Err(Stop::error("server_error"));
        }
        if !status.is_success() {
            return Err(Stop::error("http_status"));
        }
        let cap = limits.max_body_bytes.saturating_sub(bytes_read);
        let body = match read_capped(&mut response, cap).await {
            Ok(body) => body,
            Err(BodyError::TooLarge) => return Err(Stop::error("body_too_large")),
            Err(BodyError::Timeout) => return Err(Stop::new(State::Partial, Some("timeout"))),
            Err(BodyError::Transport) => {
                return Err(Stop::new(State::Offline, Some("unreachable")))
            }
        };
        bytes_read += body.len();
        let page: Page =
            serde_json::from_slice(&body).map_err(|_| Stop::error("unexpected_response"))?;
        let received = page.usage_events_display.len();
        events.extend(page.usage_events_display);
        let reached_total = page
            .total_usage_events_count
            .is_some_and(|total| events.len() as u64 >= total);
        if received < limits.page_size as usize || reached_total {
            return Ok(events);
        }
    }
    Err(Stop::error("too_many_pages"))
}

/// A walk failure covers the account for the round (a second session would
/// only repeat it: 429 amplification), except one specific to this
/// profile's session or to this machine: expired (local expiry, 401,
/// redirect, 403 JSON), disabled (removed mid-walk), client_unavailable,
/// write_failed.
fn covers_account(stop: Stop) -> bool {
    !matches!(stop.state, State::Expired | State::Disabled)
        && !matches!(stop.reason, Some("client_unavailable" | "write_failed"))
}

/// One profile's sync, start to finish. `enabled` is the live switch (and,
/// in production, "this profile is still configured as when the round
/// started"). `claim(stem, false)` returns the index of an earlier profile
/// that already covered the same account this round; `claim(stem, true)`
/// records this one: after its file is committed, or after a walk failure
/// that is not this session's own (see the module doc). `checked`: an added
/// profile, read with `read_login_checked` (a symlinked database refused);
/// the default profile follows a symlinked `state.vscdb`, as every release
/// did, and opens the resolved file the checked way without an owner check
/// (`read_login_resolved`).
async fn run(
    target: &Target<'_>,
    db_path: &Path,
    dir: &Path,
    limits: Limits,
    enabled: &dyn Fn() -> bool,
    claim: &mut dyn FnMut(&str, bool) -> Option<usize>,
    checked: bool,
) -> Outcome {
    let stopped = Outcome::stopped;
    if !enabled() {
        return stopped(Stop::new(State::Disabled, None));
    }
    let read = if checked {
        cursor_desktop::read_login_checked(db_path)
    } else {
        cursor_desktop::read_login_resolved(db_path)
    };
    let login = match read {
        Ok(Some(login)) => login,
        Ok(None) => return stopped(Stop::new(State::NotSignedIn, None)),
        Err(reason) => return Outcome::refused(Stop::error(reason)),
    };
    let (user, exp) = match token_user(&login) {
        Ok(found) => found,
        Err(stop) => return Outcome::refused(stop),
    };
    // C0.5: Cursor refreshes the token only while it runs, and an expired
    // one is answered with a redirect to WorkOS. Decide locally; send nothing.
    if exp <= now_secs() + EXP_SKEW_SECS {
        return stopped(Stop::new(State::Expired, None));
    }
    let Some(stem) = (target.file_stem)(&user) else {
        return Outcome::refused(Stop::error("key_unavailable"));
    };
    // B1: an account already covered this round by an earlier profile is
    // not walked again (saves requests, avoids 429); its file name is the
    // same, so the takeover keeps one copy. Any other account's file this
    // profile still holds (it was signed in to that one before) goes, under
    // its lock, the way a commit drops superseded files; otherwise the
    // takeover would keep counting that old account (MA-22). This
    // account's own copy stays: it is the same file name, so the takeover
    // still keeps only the newest.
    if let Some(first) = claim(&stem, false) {
        // The switch and the appDir are re-checked first: nothing is deleted
        // for a profile or appDir that is no longer wanted.
        if !enabled() {
            return stopped(Stop::new(State::Disabled, None));
        }
        let cleaned = locked_removal(dir, Some(&format!("usage.{stem}.json")), false);
        return Outcome {
            duplicate_of: Some(first),
            changed: cleaned.usage > 0,
            cleanup_failed: cleaned.failed,
            ..stopped(Stop::new(State::Duplicate, None))
        };
    }
    let Ok(mut cookie) = reqwest::header::HeaderValue::from_str(&format!(
        "WorkosCursorSessionToken={user}%3A%3A{}",
        login.access_token
    )) else {
        return Outcome::refused(Stop::error("token_unreadable"));
    };
    cookie.set_sensitive(true);
    drop(login);
    let events = match walk(target, &cookie, limits, enabled).await {
        Ok(events) => events,
        Err(stop) => {
            if covers_account(stop) {
                claim(&stem, true);
            }
            return stopped(stop);
        }
    };
    drop(cookie);
    match commit_file(dir, &stem, &events, enabled) {
        Ok(committed) => {
            claim(&stem, true);
            Outcome {
                events: events.len(),
                changed: committed.changed,
                cleanup_failed: committed.cleanup_failed,
                ..stopped(Stop::new(State::Ok, None))
            }
        }
        Err(stop) => stopped(stop),
    }
}

// ---------------------------------------------------------------------------
// Entry point (single-flight).

/// What the last round left, behind the single-flight lock. A caller
/// arriving while a sync runs waits for it and returns its status instead of
/// starting another.
struct LastRound {
    status: Option<serde_json::Value>,
    /// The slots (`Slot::key`: id and root) whose outcome wrote no file
    /// however long the round ran (`writes_no_file`). A slot with another
    /// root is not in it, so its first round may take the full budget.
    no_file: Vec<(String, Option<PathBuf>)>,
}

static IN_FLIGHT: Mutex<LastRound> = Mutex::new(LastRound {
    status: None,
    no_file: Vec::new(),
});

#[cfg(test)]
thread_local! {
    /// The budget the last round on this thread chose.
    static ROUND_BUDGET: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

impl Slot {
    /// A profile as the next round recognises it: its id and root. The dir
    /// is derived from the id, so it is not part of the key.
    fn key(&self) -> (String, Option<PathBuf>) {
        (self.id.clone(), self.root.clone())
    }
}

/// The round's budget: full for "Sync now" and while some profile has no
/// complete file but could write one; background otherwise. A profile whose last round could not write a
/// file however long it ran (`writes_no_file`) does not hold every later
/// round at the full budget.
fn round_budget(explicit: bool, slots: &[Slot], no_file: &[(String, Option<PathBuf>)]) -> Duration {
    let wants_full = slots
        .iter()
        .any(|slot| complete_file(&slot.dir).is_none() && !no_file.contains(&slot.key()));
    if explicit || wants_full {
        FULL_BUDGET
    } else {
        BACKGROUND_BUDGET
    }
}

/// Run one sync against Cursor. Returns the status JSON and whether a synced
/// file changed (the caller then invalidates scan caches).
pub(crate) fn sync_now(explicit: bool, home: Option<&Path>) -> (serde_json::Value, bool) {
    let stem = |user: &str| {
        crate::agent_account_scope::installation_digest_hex(FILE_NAME_DOMAIN, user.as_bytes()).ok()
    };
    let db_path = cursor_desktop::state_db_path();
    sync_with(
        explicit,
        home,
        &Target {
            db_path: &db_path,
            url: USAGE_EVENTS_URL,
            https_only: true,
            file_stem: &stem,
        },
        &config,
        None,
    )
}

/// The round's headline state for callers that show one line: `ok` when
/// every profile is `ok` or `duplicate`, else the first other state in
/// profile order (default first). A profile with no login (`notSignedIn`) is
/// not a problem while another profile synced.
fn headline(outcomes: &[Outcome]) -> Stop {
    let fine = |o: &Outcome| matches!(o.stop.state, State::Ok | State::Duplicate);
    let any_fine = outcomes.iter().any(fine);
    outcomes
        .iter()
        .find(|o| !fine(o) && !(any_fine && o.stop.state == State::NotSignedIn))
        .map(|o| o.stop)
        .unwrap_or(Stop::new(State::Ok, None))
}

fn sync_with(
    explicit: bool,
    home: Option<&Path>,
    target: &Target<'_>,
    config: &(dyn Fn() -> Config + Sync),
    test_limits: Option<Limits>,
) -> (serde_json::Value, bool) {
    let mut last = match IN_FLIGHT.try_lock() {
        Ok(last) => last,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            let last = IN_FLIGHT.lock().unwrap_or_else(|p| p.into_inner());
            return (
                last.status.clone().unwrap_or(serde_json::Value::Null),
                false,
            );
        }
    };
    let start = config();
    // Off is decided per profile, before its login is read (`run`).
    let profiles = slots(&start, target.db_path);
    // The appDir may have changed since it was accepted (e.g. replaced by a
    // symlink into the tokscale tree). Checked at the start of the round,
    // with a fixed reason and no read from it, and again in `still_wanted`
    // before every page and under the dir lock before the write.
    let app_dir_ok = || match (start.app_dir.as_deref(), home) {
        (Some(app_dir), Some(home)) => check_app_dir(app_dir, home).is_ok(),
        _ => true,
    };
    let app_dir_rejected = !app_dir_ok();
    let budget = round_budget(explicit, &profiles, &last.no_file);
    #[cfg(test)]
    ROUND_BUDGET.with(|cell| cell.set(Some(budget)));
    let limits = test_limits.unwrap_or(Limits::production(budget));
    // One budget for the whole round, so several profiles cannot hold
    // IN_FLIGHT for longer than one would. (A database read that blocks in
    // the kernel, e.g. on a stalled network volume, is not bounded by it.)
    let deadline = Instant::now() + limits.budget;
    let mut walked: Vec<(String, usize)> = Vec::new();
    let mut outcomes: Vec<Outcome> = Vec::with_capacity(profiles.len());
    for (index, profile) in profiles.iter().enumerate() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let outcome = if app_dir_rejected {
            Outcome::refused(Stop::error("app_dir_rejected"))
        } else if remaining.is_zero() {
            Outcome::stopped(Stop::new(State::Partial, Some("budget_exhausted")))
        } else {
            let still_wanted = || profile.still_in(&start, &config()) && app_dir_ok();
            let mut claim = |stem: &str, record: bool| {
                if record {
                    walked.push((stem.to_string(), index));
                    return None;
                }
                walked
                    .iter()
                    .find(|(s, _)| s == stem)
                    .map(|(_, first)| *first)
            };
            crate::RUNTIME.block_on(run(
                target,
                &profile.db,
                &profile.dir,
                Limits {
                    budget: remaining,
                    ..limits
                },
                &still_wanted,
                &mut claim,
                profile.root.is_some(),
            ))
        };
        outcomes.push(outcome);
    }
    let changed = outcomes.iter().any(|o| o.changed);
    let head = if profiles.is_empty() {
        Stop::new(State::Disabled, None)
    } else {
        headline(&outcomes)
    };
    // Any profile that completed a walk this round holds a current usage file,
    // whether or not its bytes changed (`changed` is bytes, not completion).
    let completed_walk = outcomes.iter().any(|o| o.stop.state == State::Ok);
    let cli_present = completed_walk && takeover(&config(), home) == Takeover::Blocked;
    let state = if cli_present {
        State::CliPresent
    } else {
        head.state
    };
    // Nothing is read from a rejected appDir.
    let last_success_ms = |dir: &Path| {
        if app_dir_rejected {
            return None;
        }
        complete_file(dir)
            .and_then(|(_, modified)| modified.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
    };
    let per_profile: Vec<serde_json::Value> = profiles
        .iter()
        .zip(&outcomes)
        .map(|(profile, outcome)| {
            let mut entry = serde_json::json!({
                "id": profile.id,
                "state": outcome.stop.state,
                "events": outcome.events,
                "lastSuccessMs": last_success_ms(&profile.dir),
            });
            if let Some(reason) = outcome.stop.reason {
                entry["reason"] = reason.into();
            }
            if let Some(first) = outcome.duplicate_of.and_then(|i| profiles.get(i)) {
                entry["duplicateOf"] = first.id.clone().into();
            }
            entry
        })
        .collect();
    let mut status = serde_json::json!({
        "state": state,
        "events": outcomes.iter().map(|o| o.events).sum::<usize>(),
        "lastSuccessMs": profiles.iter().filter_map(|p| last_success_ms(&p.dir)).max(),
        "changed": changed,
        "profiles": per_profile,
        "cleanupFailedProfiles": profiles
            .iter()
            .zip(&outcomes)
            .filter(|(_, outcome)| outcome.cleanup_failed)
            .map(|(profile, _)| profile.id.as_str())
            .collect::<Vec<_>>(),
    });
    // `cliPresent` has no reason; another profile's must not ride along.
    if let Some(reason) = head.reason.filter(|_| !cli_present) {
        status["reason"] = reason.into();
    }
    last.no_file = profiles
        .iter()
        .zip(&outcomes)
        .filter(|(_, outcome)| writes_no_file(outcome))
        .map(|(profile, _)| profile.key())
        .collect();
    last.status = Some(status.clone());
    (status, changed)
}

#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

// A registry seen only by the calling thread's `config()`, so a report test
// (which builds its scan settings on its own thread) can turn the takeover
// on without enabling it for other tests through the global one.
#[cfg(test)]
thread_local! {
    static THREAD_CONFIG: std::cell::RefCell<Option<Config>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_thread_config_for_test(config: Option<Config>) {
    THREAD_CONFIG.with(|c| *c.borrow_mut() = config);
}

#[cfg(test)]
pub(crate) fn set_for_test(config: Config) {
    *CONFIG.write().unwrap_or_else(|p| p.into_inner()) = config;
    STALE_APP_DIRS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
}

#[cfg(test)]
mod tests;
