//! Cursor usage synced from the signed-in Cursor desktop app.
//!
//! Data flow, per Cursor profile (the default one plus each added
//! `--user-data-dir` root, walked one at a time): its `state.vscdb`
//! (read-only, `cursor_desktop::read_login_checked`) → access token + its
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
//! - Swift passes a bundle id and `(uuid, root)` pairs, never a directory or a
//!   database path; Rust derives both (CMP-520 B2). `sub` never crosses the
//!   FFI: a profile whose account another profile already walked reports
//!   only that profile's id (`duplicateOf`).

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
            !label.is_empty() && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
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
    match std::fs::symlink_metadata(&app_dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err("cursor sync app dir is not a real directory".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("cursor sync app dir cannot be examined".to_string()),
    }
    // D1: Syrtis never writes into the tokscale CLI's tree (and a sync file
    // there would be dropped by the takeover's own exclusion), including
    // through a symlinked ancestor.
    let tokscale = home.join(".config/tokscale");
    if app_dir.starts_with(&tokscale) || resolved(&app_dir).starts_with(resolved(&tokscale)) {
        return Err("cursor sync app dir must not be inside ~/.config/tokscale".to_string());
    }
    Ok(app_dir)
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

fn parse_config(raw: &str, home: Option<&Path>) -> Result<Config, String> {
    let input: SetInput =
        serde_json::from_str(raw).map_err(|_| "invalid cursor sync JSON".to_string())?;
    let app_dir = app_dir_for(&input.bundle_id, home)?;
    if input.profiles.len() > MAX_PROFILES {
        return Err("too many cursor profiles".to_string());
    }
    let mut profiles: Vec<Profile> = Vec::with_capacity(input.profiles.len());
    for profile in input.profiles {
        if !valid_profile_id(&profile.id) || profiles.iter().any(|p| p.id == profile.id) {
            return Err("cursor profile id is invalid".to_string());
        }
        profiles.push(Profile {
            id: profile.id,
            root: validate_root(&profile.root)?,
        });
    }
    Ok(Config {
        enabled: input.enabled,
        app_dir: Some(app_dir),
        profiles,
        cli_takeover_confirmed: input.cli_takeover_confirmed,
    })
}

/// Replace the registry from
/// `{"enabled":bool,"bundleId":"a.b","profiles":[{"id":"<UUID>","root":"/abs"}],"cliTakeoverConfirmed":bool}`.
/// Invalid input leaves the registry untouched. Cleanup (P3-7, B2): turning
/// sync off deletes our files from every profile dir; while on, dirs of
/// profiles no longer listed (orphans) are emptied of our files and removed.
pub(crate) fn set_from_json(raw: &str, home: Option<&Path>) -> Result<serde_json::Value, String> {
    let new = parse_config(raw, home)?;
    let old = {
        let mut guard = CONFIG
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::replace(&mut *guard, new.clone())
    };
    let mut cleanup = Cleanup::default();
    if let Some(old_app) = old.app_dir.as_deref() {
        if new.app_dir.as_deref() != Some(old_app) {
            cleanup.everything(old_app);
        }
    }
    let app_dir = new.app_dir.as_deref().unwrap_or(Path::new(""));
    if new.enabled {
        cleanup.orphans(app_dir, &new.profiles);
    } else {
        cleanup.everything(app_dir);
    }
    let profiles: Vec<_> = slots(&new, Path::new(""))
        .into_iter()
        .map(|slot| serde_json::json!({"id": slot.id, "dir": slot.dir.to_string_lossy()}))
        .collect();
    Ok(serde_json::json!({
        "enabled": new.enabled,
        "appDir": app_dir.to_string_lossy(),
        "profiles": profiles,
        "cliTakeoverConfirmed": new.cli_takeover_confirmed,
        "removedFiles": cleanup.removed,
        "cleanupFailed": !cleanup.failed.is_empty(),
        "cleanupFailedProfiles": cleanup.failed,
    }))
}

#[derive(Default)]
struct Cleanup {
    removed: usize,
    /// Profile ids (`default` or a UUID) whose files could not all be deleted.
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
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
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

/// Delete Syrtis usage files (and temp leftovers) in `dir` except `keep`.
/// Never touches anything else. Caller holds the dir lock. Returns the count
/// removed and whether any of ours could not be (or the dir not be read).
fn remove_usage_files(dir: &Path, keep: Option<&str>) -> (usize, bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, true);
    };
    let (mut removed, mut failed) = (0, false);
    for entry in entries {
        let Ok(entry) = entry else {
            failed = true;
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !is_ours(&name) || Some(name.as_ref()) == keep {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => failed = true,
        }
    }
    (removed, failed)
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
    // Only a real directory: never follow a symlink to delete elsewhere.
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return (0, false),
        Err(error) => return (0, error.kind() != std::io::ErrorKind::NotFound),
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
        return (0, holds_ours);
    }
    let (removed, failed) = remove_usage_files(dir, None);
    if remove_dir && !failed {
        let _ = std::fs::remove_file(dir.join(LOCK_FILE_NAME));
        let _ = std::fs::remove_dir(dir);
    }
    (removed, failed)
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

/// Write the complete walk: dir 0700, temp 0600, fsync, atomic rename, dir
/// fsync, then drop every other usage file. `still_wanted` is re-checked
/// under the lock so a concurrent disable (which deletes under the same lock)
/// can never be undone by a walk finishing after it.
fn commit_file(
    dir: &Path,
    stem: &str,
    events: &[Event],
    still_wanted: &dyn Fn() -> bool,
) -> Result<(), Stop> {
    let failed = || Stop::error("write_failed");
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
    let _ = remove_usage_files(dir, Some(&final_name));
    Ok(())
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
    /// The account was already walked this round from an earlier profile.
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
    /// already walked the same account. Never the account itself.
    duplicate_of: Option<usize>,
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

/// One profile's sync, start to finish. `enabled` is the live switch (and,
/// in production, "this profile is still configured as when the round
/// started"). `claim` records the account's file stem for this round and
/// returns the index of an earlier profile that already walked it.
async fn run(
    target: &Target<'_>,
    db_path: &Path,
    dir: &Path,
    limits: Limits,
    enabled: &dyn Fn() -> bool,
    claim: &mut dyn FnMut(&str) -> Option<usize>,
) -> Outcome {
    let stopped = |stop| Outcome {
        stop,
        events: 0,
        duplicate_of: None,
    };
    if !enabled() {
        return stopped(Stop::new(State::Disabled, None));
    }
    let login = match cursor_desktop::read_login_checked(db_path) {
        Ok(Some(login)) => login,
        Ok(None) => return stopped(Stop::new(State::NotSignedIn, None)),
        Err(reason) => return stopped(Stop::error(reason)),
    };
    let (user, exp) = match token_user(&login) {
        Ok(found) => found,
        Err(stop) => return stopped(stop),
    };
    // C0.5: Cursor refreshes the token only while it runs, and an expired
    // one is answered with a redirect to WorkOS. Decide locally; send nothing.
    if exp <= now_secs() + EXP_SKEW_SECS {
        return stopped(Stop::new(State::Expired, None));
    }
    let Some(stem) = (target.file_stem)(&user) else {
        return stopped(Stop::error("key_unavailable"));
    };
    // B1: the same account in a second profile is not walked again this
    // round (saves requests, avoids 429); its file name is the same, so the
    // takeover keeps one copy.
    if let Some(first) = claim(&stem) {
        return Outcome {
            stop: Stop::new(State::Duplicate, None),
            events: 0,
            duplicate_of: Some(first),
        };
    }
    let Ok(mut cookie) = reqwest::header::HeaderValue::from_str(&format!(
        "WorkosCursorSessionToken={user}%3A%3A{}",
        login.access_token
    )) else {
        return stopped(Stop::error("token_unreadable"));
    };
    cookie.set_sensitive(true);
    drop(login);
    let events = match walk(target, &cookie, limits, enabled).await {
        Ok(events) => events,
        Err(stop) => return stopped(stop),
    };
    drop(cookie);
    match commit_file(dir, &stem, &events, enabled) {
        Ok(()) => Outcome {
            stop: Stop::new(State::Ok, None),
            events: events.len(),
            duplicate_of: None,
        },
        Err(stop) => stopped(stop),
    }
}

// ---------------------------------------------------------------------------
// Entry point (single-flight).

/// The last status, behind the single-flight lock. A caller arriving while a
/// sync runs waits for it and returns its status instead of starting another.
static IN_FLIGHT: Mutex<Option<serde_json::Value>> = Mutex::new(None);

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
    let mut slot = match IN_FLIGHT.try_lock() {
        Ok(slot) => slot,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            let slot = IN_FLIGHT.lock().unwrap_or_else(|p| p.into_inner());
            return (slot.clone().unwrap_or(serde_json::Value::Null), false);
        }
    };
    let start = config();
    // Off is decided per profile, before its login is read (`run`).
    let profiles = slots(&start, target.db_path);
    let budget = if explicit || profiles.iter().any(|p| complete_file(&p.dir).is_none()) {
        FULL_BUDGET
    } else {
        BACKGROUND_BUDGET
    };
    let limits = test_limits.unwrap_or(Limits::production(budget));
    // One budget for the whole round, so several profiles cannot hold
    // IN_FLIGHT for longer than one would. (A database read that blocks in
    // the kernel, e.g. on a stalled network volume, is not bounded by it.)
    let deadline = Instant::now() + limits.budget;
    let mut walked: Vec<(String, usize)> = Vec::new();
    let mut outcomes: Vec<Outcome> = Vec::with_capacity(profiles.len());
    for (index, profile) in profiles.iter().enumerate() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let outcome = if remaining.is_zero() {
            Outcome {
                stop: Stop::new(State::Partial, Some("budget_exhausted")),
                events: 0,
                duplicate_of: None,
            }
        } else {
            let still_wanted = || profile.still_in(&start, &config());
            let mut claim = |stem: &str| {
                if let Some((_, first)) = walked.iter().find(|(s, _)| s == stem) {
                    return Some(*first);
                }
                walked.push((stem.to_string(), index));
                None
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
            ))
        };
        outcomes.push(outcome);
    }
    let changed = outcomes.iter().any(|o| o.stop.state == State::Ok);
    let head = if profiles.is_empty() {
        Stop::new(State::Disabled, None)
    } else {
        headline(&outcomes)
    };
    let state = if changed && takeover(&config(), home) == Takeover::Blocked {
        State::CliPresent
    } else {
        head.state
    };
    let last_success_ms = |dir: &Path| {
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
        "profiles": per_profile,
    });
    if let Some(reason) = head.reason {
        status["reason"] = reason.into();
    }
    *slot = Some(status.clone());
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
}

#[cfg(test)]
mod tests;
