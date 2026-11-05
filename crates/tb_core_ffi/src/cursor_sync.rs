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
//! - A sealed token (`cursor-safe-storage:v1:`, Cursor 3.27+) is opened with
//!   Cursor's Safe Storage key only while the user's "cursor" Keychain
//!   consent stands; a plaintext token never consults consent or the
//!   Keychain. The Keychain is read at most once per round, and only by a
//!   round the user started or the process's first round; the derived key
//!   stays in memory until the grant is removed or sync is turned off
//!   (plan CURSOR-SS-M, `SealedRound`). Every outcome is a fixed reason:
//!   `keychain_consent`, `keychain_denied`, `keychain_missing`,
//!   `keychain_unavailable`, `keychain_unlock`, `token_sealed_unreadable`,
//!   `token_sealed_unsupported`.
//! - Swift passes a bundle id and `(uuid, root)` pairs, never a directory or a
//!   database path; Rust derives both (CMP-520 B2). `sub` never crosses the
//!   FFI: a profile whose account an earlier profile already covered this
//!   round reports only that profile's id (`duplicateOf`). A walk covers its
//!   account when it synced, or failed in a way a second session would only
//!   repeat (`covers_account`): every failure except one specific to that
//!   profile's session or to this machine — expired (local expiry, 401,
//!   redirect, 403 JSON), disabled (removed mid-walk), client_unavailable,
//!   write_failed — which leaves the account to the next profile on it.
//!
//! Billing V2 (plan CURSOR-BV2-M r4): the old walk runs first every round and
//! keeps every result it has today. Only when its page 1 is a 2xx JSON object
//! without `usageEventsDisplay` does that profile's round read
//! `query-billing-items` (`cursor_billing_v2`) with the same client and
//! cookie. The live `usage.<stem>.json` stays the one file the engine reads;
//! beside it, the dot-prefixed sidecar `.cursor-sync.history.<stem>.json`
//! (never scanned) keeps the old rows frozen at the first V2 commit, the
//! cutover, and the months V2 has read (`History`). A V2 commit writes
//! frozen rows ∪ V2 rows after the cutover; an old-walk commit deletes the
//! sidecar (the full old history is back). Fixed reasons:
//! `billing_v2_team_required`, `billing_v2_unavailable`,
//! `billing_v2_no_items`, `billing_v2_principal_mismatch`.

use crate::cursor_billing_v2::{self, Fetched};
use crate::cursor_desktop::{self, CursorLogin};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const USAGE_EVENTS_URL: &str = "https://cursor.com/api/dashboard/get-filtered-usage-events";
const FILE_NAME_DOMAIN: &str = "cursor-sync-file";
const LOCK_FILE_NAME: &str = ".cursor-sync.lock";
/// Starts with a dot so it can never match the engine's `usage*` patterns: a
/// half-written temp file must not be scanned.
const TEMP_FILE_PREFIX: &str = ".cursor-sync.tmp-";
/// The Billing V2 history sidecar, `.cursor-sync.history.<stem>.json`: dot
/// prefixed (never one of the engine's `usage.*` files) and not the temp
/// prefix (which every commit deletes).
const SIDECAR_PREFIX: &str = ".cursor-sync.history.";

const PAGE_SIZE: u32 = 500;
/// Upstream's per-page timeout (`CURSOR_HTTP_TIMEOUT`), clamped to the budget.
const PAGE_TIMEOUT: Duration = Duration::from_secs(15);
/// 256 pages × 500 = 128k events. The byte cap below normally binds first.
const MAX_PAGES: u32 = 256;
/// Billing V2 pages hold 100 items (`cursor_billing_v2::PAGE_SIZE`), a fifth
/// of the old 500: 1280 pages keep the old walk's ceiling of 128k events,
/// counted over every window of one walk (S4). Unmeasured: no V2 history of
/// that size has been seen; at ~1 s/page the round's budget binds first.
const MAX_V2_PAGES: u32 = 1280;
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
    // Sync turned off: the cached Cursor key goes with it (U3, Design 4).
    if !new.enabled {
        drop_sealed_key();
    }
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

/// What a delete pass did: `removed` counts every file of ours (usage files,
/// temp leftovers, history sidecars), `usage` only the usage files, which are
/// what the engine reads (a temp file or a sidecar never changes what is
/// counted).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Removal {
    removed: usize,
    usage: usize,
    failed: bool,
}

/// Delete Syrtis usage files (and temp leftovers and history sidecars) in
/// `dir` except the names in `keep`. Never touches anything else. Caller
/// holds the dir lock. `failed`: any of ours could not be deleted, or the dir
/// could not be read. `spare_sidecar_dirs`: the old walk's commit only.
fn remove_usage_files(dir: &Path, keep: &[&str], spare_sidecar_dirs: bool) -> Removal {
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
        if !is_ours(&name) || keep.contains(&name.as_ref()) {
            continue;
        }
        // Syrtis only ever writes a sidecar as a file: a directory with that
        // name must not turn an old walk's otherwise unchanged commit into a
        // cleanup failure. Disable and profile removal still try it, and
        // report it when it cannot go.
        if spare_sidecar_dirs
            && is_sidecar_name(&name)
            && entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
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

/// B2: exactly our file names, `usage.<64 hex>.json`, our temp prefix and
/// `.cursor-sync.history.<64 hex>.json` (a sidecar of any stem).
fn is_ours(name: &str) -> bool {
    is_sync_file_name(name) || name.starts_with(TEMP_FILE_PREFIX) || is_sidecar_name(name)
}

fn is_sidecar_name(name: &str) -> bool {
    name.strip_prefix(SIDECAR_PREFIX)
        .and_then(|rest| rest.strip_suffix(".json"))
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn live_name(stem: &str) -> String {
    format!("usage.{stem}.json")
}

fn sidecar_name(stem: &str) -> String {
    format!("{SIDECAR_PREFIX}{stem}.json")
}

/// `(removed, failed)`. A missing dir, or one that is not a real directory,
/// holds nothing of ours: nothing to delete, not a failure. A dir that cannot
/// be examined, or whose files of ours cannot be deleted, is a failure.
/// `remove_dir`: an orphaned profile dir; once our files are gone, drop our
/// lock file and `rmdir` it, which succeeds only if nothing else is left
/// (never `remove_dir_all`).
fn remove_usage_files_locked(dir: &Path, remove_dir: bool) -> (usize, bool) {
    let result = locked_removal(dir, &[], remove_dir);
    (result.removed, result.failed)
}

/// `remove_usage_files_locked` that spares `keep` (file names) and reports
/// the usage files separately.
fn locked_removal(dir: &Path, keep: &[&str], remove_dir: bool) -> Removal {
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
    let result = remove_usage_files(dir, keep, false);
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
    let _lock = lock_dir(dir, still_wanted)?;
    // An old walk brings the full old history back, so this stem's Billing
    // V2 sidecar goes, before the live file is replaced. Best effort: the old
    // walk's commit never fails on it, and a sidecar that outlives this
    // commit no longer matches the live file's SHA-256: it is stale, and the
    // next V2 commit freezes this file afresh (`commit_v2`). A non-file at
    // that name never gets here (`run` refuses the round first).
    let _ = std::fs::remove_file(dir.join(sidecar_name(stem)));
    let payload = serde_json::to_vec(&UsageFile {
        total_usage_events_count: events.len(),
        usage_events_display: events,
    })
    .map_err(|_| failed())?;
    let final_name = live_name(stem);
    let changed = write_atomic(dir, &final_name, &payload)?;
    let removal = remove_usage_files(dir, &[&final_name], true);
    Ok(Committed {
        changed: changed || removal.usage > 0,
        cleanup_failed: removal.failed,
    })
}

/// `dir` as a real directory, locked exclusively (released when the returned
/// file drops); `still_wanted` checked before anything is created and again
/// under the lock (see `commit_file`).
fn lock_dir(dir: &Path, still_wanted: &dyn Fn() -> bool) -> Result<std::fs::File, Stop> {
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
    Ok(lock)
}

/// `dir/name` holds `payload` afterwards: unchanged bytes only get a new
/// mtime (`refresh_if_same`); otherwise temp 0600, fsync, atomic rename, dir
/// fsync. Returns whether the bytes changed. Caller holds the dir lock.
fn write_atomic(dir: &Path, name: &str, payload: &[u8]) -> Result<bool, Stop> {
    if refresh_if_same(&dir.join(name), payload) {
        return Ok(false);
    }
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
        file.write_all(payload)?;
        file.sync_all()?;
        drop(file);
        tokscale_core::fs_atomic::replace_file(&temp_path, &dir.join(name))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temp_path);
        return Err(Stop::error("write_failed"));
    }
    // The rename is the commit point; a failed dir fsync cannot undo it.
    let _ = crate::agent_quota_history::sync_directory(dir);
    Ok(true)
}

// ---------------------------------------------------------------------------
// Billing V2 history (plan CURSOR-BV2-M r4, design 3-5b, D5, D6).

/// The sidecar `.cursor-sync.history.<stem>.json`. The first V2 commit for a
/// stem creates it from that stem's live file. While the live file is the
/// one its last V2 commit wrote (`live_sha256`), `frozen` and `cutover_ms`
/// never change, only `fetched` and `live_sha256`. Every V2 commit writes
/// the live file as `frozen` followed by its V2 rows, so those V2 rows are
/// the ones after the first `frozen.len()`. A live file another writer
/// replaced (an old walk whose sidecar delete failed, an older build) makes
/// the sidecar stale: it is never trusted, and the next V2 commit freezes
/// again from that live file, which then holds the old endpoint's history.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct History {
    /// The old endpoint's rows as they were when this sidecar was made.
    frozen: Vec<Event>,
    /// V2 rows at or before it are never written. None: no old rows.
    cutover_ms: Option<i64>,
    /// UTC months (`cursor_billing_v2::month_index`) V2 has read to their
    /// last page.
    fetched: std::collections::BTreeSet<i32>,
    /// SHA-256 (hex) of the live file the last V2 commit wrote.
    #[serde(default)]
    live_sha256: Option<String>,
}

/// `History` as written, its frozen rows borrowed from the live file's rows.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoryOut<'a> {
    frozen: &'a [Event],
    cutover_ms: Option<i64>,
    fetched: &'a std::collections::BTreeSet<i32>,
    live_sha256: &'a str,
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The stem's live file: its rows and the SHA-256 of its bytes.
struct Live {
    rows: Vec<Event>,
    sha256: Option<String>,
}

impl History {
    /// This sidecar describes the live file (its last V2 commit wrote it).
    fn trusts(&self, live: &Live) -> bool {
        self.live_sha256.is_some() && self.live_sha256 == live.sha256
    }
}

/// The stem's sidecar: None when there is none; Err when one is there but is
/// not a regular file we can read and parse (fail closed: no round).
fn read_history(dir: &Path, stem: &str) -> Result<Option<History>, ()> {
    let path = dir.join(sidecar_name(stem));
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Ok(meta) if meta.is_file() => {
            let bytes = std::fs::read(&path).map_err(|_| ())?;
            serde_json::from_slice(&bytes).map(Some).map_err(|_| ())
        }
        _ => Err(()),
    }
}

/// The stem's live file; no rows and no digest when it is absent or not a
/// regular file (never followed, as in `refresh_if_same`); Err when it
/// cannot be parsed.
fn read_live(dir: &Path, stem: &str) -> Result<Live, ()> {
    let path = dir.join(live_name(stem));
    if !std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file()) {
        return Ok(Live {
            rows: Vec::new(),
            sha256: None,
        });
    }
    let bytes = std::fs::read(&path).map_err(|_| ())?;
    let rows = serde_json::from_slice::<Page>(&bytes)
        .map(|page| page.usage_events_display)
        .map_err(|_| ())?;
    Ok(Live {
        rows,
        sha256: Some(sha256_hex(&bytes)),
    })
}

/// Unix ms from a timestamp the engine accepts: base-10 ms, else RFC 3339
/// (engine `parse_ms_timestamp`).
pub(crate) fn timestamp_ms(text: &str) -> Option<i64> {
    let text = text.trim();
    text.parse::<i64>().ok().or_else(|| {
        chrono::DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|time| time.timestamp_millis())
    })
}

fn event_ms(event: &Event) -> Option<i64> {
    match event.timestamp.as_ref()? {
        Timestamp::Text(text) => timestamp_ms(text),
        Timestamp::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|ms| ms as i64)),
    }
}

/// D5: the newest frozen time, clamped to now (S8: a future timestamp must
/// not hold V2 rows back); now when no frozen time can be read; None without
/// frozen rows.
fn cutover_of(frozen: &[Event], now_ms: i64) -> Option<i64> {
    if frozen.is_empty() {
        return None;
    }
    Some(
        frozen
            .iter()
            .filter_map(event_ms)
            .max()
            .map_or(now_ms, |newest| newest.min(now_ms)),
    )
}

/// The months a V2 round reads, newest first; whether it stops after 3
/// empty calendar months in a row; and the cutover it plans with.
struct Plan {
    windows: Vec<i32>,
    stop_after_empty: bool,
    cutover: Option<i64>,
}

/// Design 5/6. Range: the cutover's month (or 11 months back, whichever is
/// later) to now. An explicit round reads all of it with no early stop; a
/// background one the last three months (not before the range) plus any
/// month V2 never read, and with no cutover stops after 3 empty months.
/// `history` is the trusted sidecar (None: none, or `stale`); a stale one
/// plans as a first commit would (cutover from the live file, nothing read
/// yet), with no early stop, so no month V2 had read drops out. Read
/// without the lock; the commit re-reads both files under it.
fn v2_windows(
    history: Option<&History>,
    live: &[Event],
    stale: bool,
    explicit: bool,
    now_ms: i64,
) -> Plan {
    let (cutover, fetched) = match history {
        Some(history) => (history.cutover_ms, Some(&history.fetched)),
        None => (cutover_of(live, now_ms), None),
    };
    let stop_after_empty = cutover.is_none() && !explicit && !stale;
    let Some(current) = cursor_billing_v2::month_index(now_ms) else {
        return Plan {
            windows: Vec::new(),
            stop_after_empty,
            cutover,
        };
    };
    let floor = current - 11;
    let first = cutover
        .and_then(cursor_billing_v2::month_index)
        .map_or(floor, |month| month.max(floor));
    let recent = first.max(current - 2);
    let windows = (first..=current)
        .rev()
        .filter(|month| {
            explicit || *month >= recent || !fetched.is_some_and(|read| read.contains(month))
        })
        .collect();
    Plan {
        windows,
        stop_after_empty,
        cutover,
    }
}

/// Design 4/5b, D6: under the dir lock, the live file becomes frozen rows ∪
/// the live file's V2 rows in months this walk did not re-read with rows ∪
/// this walk's rows after the cutover. With no sidecar, or a stale one (the
/// live file is not the one its last V2 commit wrote), the sidecar is made
/// afresh from this live file: frozen = its rows, cutover = min(newest,
/// now), nothing read yet. A month re-read with no rows keeps its rows.
/// Nothing at all to write (no old rows, no V2 rows): `billing_v2_no_items`,
/// nothing written. Returns the commit and the live file's row count.
fn commit_v2(
    dir: &Path,
    stem: &str,
    fetched: Fetched,
    now_ms: i64,
    still_wanted: &dyn Fn() -> bool,
) -> Result<(Committed, usize), Stop> {
    let failed = || Stop::error("write_failed");
    let _lock = lock_dir(dir, still_wanted)?;
    let live = read_live(dir, stem).map_err(|_| failed())?;
    let stored = read_history(dir, stem).map_err(|_| failed())?;
    let trusted = stored.as_ref().is_some_and(|history| history.trusts(&live));
    let (mut history, live_v2) = match stored {
        Some(history) if trusted => {
            let v2: Vec<Event> = live.rows.into_iter().skip(history.frozen.len()).collect();
            (history, v2)
        }
        // No sidecar, or a stale one: never keep stale frozen rows over a
        // newer old-walk file; freeze the live file as it now stands.
        _ => (
            History {
                cutover_ms: cutover_of(&live.rows, now_ms),
                frozen: live.rows,
                fetched: std::collections::BTreeSet::new(),
                live_sha256: None,
            },
            Vec::new(),
        ),
    };
    let cutover = history.cutover_ms;
    let fresh: Vec<_> = fetched
        .rows
        .into_iter()
        .filter(|row| cutover.is_none_or(|cutover| row.ms > cutover))
        .collect();
    let with_rows: std::collections::BTreeSet<i32> = fresh.iter().map(|row| row.month).collect();
    let months = fetched.months;
    history.fetched.extend(months.iter().copied());
    let replaced = |event: &Event| {
        event_ms(event)
            .and_then(cursor_billing_v2::month_index)
            .is_some_and(|month| with_rows.contains(&month))
    };
    let frozen_len = history.frozen.len();
    let mut events = history.frozen;
    events.extend(live_v2.into_iter().filter(|event| !replaced(event)));
    events.extend(fresh.into_iter().map(|row| row.event));
    if events.is_empty() {
        return Err(Stop::error("billing_v2_no_items"));
    }
    let payload = serde_json::to_vec(&UsageFile {
        total_usage_events_count: events.len(),
        usage_events_display: &events,
    })
    .map_err(|_| failed())?;
    let live_sha256 = sha256_hex(&payload);
    let sidecar = serde_json::to_vec(&HistoryOut {
        frozen: &events[..frozen_len],
        cutover_ms: cutover,
        fetched: &history.fetched,
        live_sha256: &live_sha256,
    })
    .map_err(|_| failed())?;
    let sidecar_name = sidecar_name(stem);
    let final_name = live_name(stem);
    // The live file first. Should the sidecar write then fail, the previous
    // sidecar (or none) no longer matches this live file, so the next
    // commit freezes this file whole (cutover at its newest row): nothing
    // counted twice.
    let changed = write_atomic(dir, &final_name, &payload)?;
    write_atomic(dir, &sidecar_name, &sidecar)?;
    let removal = remove_usage_files(dir, &[&final_name, &sidecar_name], false);
    Ok((
        Committed {
            changed: changed || removal.usage > 0,
            cleanup_failed: removal.failed,
        },
        events.len(),
    ))
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
pub(crate) enum Timestamp {
    Text(String),
    Number(serde_json::Number),
}

/// A count or cents value. The engine coerces a number or a numeric string
/// (`de_opt_*_lenient`), so both are accepted here; anything else (an object,
/// a non-numeric string) fails the page rather than carrying free text into
/// the file. Written back as a JSON number.
#[derive(Serialize)]
#[serde(transparent)]
pub(crate) struct Num(pub(crate) serde_json::Number);

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
pub(crate) struct TokenUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) input_tokens: Option<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) output_tokens: Option<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cache_read_tokens: Option<Num>,
    /// Read by the engine (`cache_write`), though absent on many events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cache_write_tokens: Option<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) total_cents: Option<Num>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Event {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) timestamp: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) charged_cents: Option<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) token_usage: Option<TokenUsage>,
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
pub(crate) struct Stop {
    state: State,
    reason: Option<&'static str>,
}

impl Stop {
    pub(crate) fn new(state: State, reason: Option<&'static str>) -> Self {
        Self { state, reason }
    }
    pub(crate) fn error(reason: &'static str) -> Self {
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
    /// Billing V2 rows of this account left out for an unreadable
    /// `eventTime` or no model (reported as `droppedRows`; never a row).
    dropped: usize,
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
            dropped: 0,
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
pub(crate) struct Limits {
    pub(crate) budget: Duration,
    pub(crate) page_timeout: Duration,
    page_size: u32,
    /// The old walk's page cap.
    pub(crate) max_pages: u32,
    /// The V2 walk's own page cap (`MAX_V2_PAGES`), over all its windows
    /// (S4). The byte cap is per walk too, old or V2.
    pub(crate) max_v2_pages: u32,
    pub(crate) max_body_bytes: usize,
    /// The round was started by the user: a V2 walk re-reads every month in
    /// its range (design 6), not only the recent and never-read ones.
    explicit: bool,
}

impl Limits {
    fn production(budget: Duration) -> Self {
        Self {
            budget,
            page_timeout: PAGE_TIMEOUT,
            page_size: PAGE_SIZE,
            max_pages: MAX_PAGES,
            max_v2_pages: MAX_V2_PAGES,
            max_body_bytes: MAX_BODY_BYTES,
            explicit: false,
        }
    }
}

/// Everything one sync depends on. Production builds it from constants;
/// tests inject a local server, a fixture database and a fixed key.
struct Target<'a> {
    /// The default profile's database; added profiles derive their own.
    db_path: &'a Path,
    url: &'a str,
    /// Billing V2's base URL (production: `cursor_billing_v2::BILLING_ITEMS_URL`).
    billing_url: &'a str,
    https_only: bool,
    file_stem: &'a dyn Fn(&str) -> Option<String>,
    /// The per-account fetch of Cursor's Safe Storage password (production:
    /// `macos_safe_storage::cursor_keychain_fetch`), or `None` where there is
    /// no key source: a sealed token is then `token_sealed_unsupported`.
    key_fetch: Option<&'a KeyFetch<'a>>,
    /// The user's Cursor Keychain consent (production:
    /// `keychain_consent::allowed("cursor")`), re-read at every use.
    consent: &'a dyn Fn() -> bool,
}

/// Account name → password bytes, or the Keychain's OSStatus.
type KeyFetch<'a> = dyn Fn(&str) -> Result<Vec<u8>, i32> + 'a;

// ---------------------------------------------------------------------------
// Sealed tokens (plan CURSOR-SS-M, Design 3-4, U3).
//
// The derived key stays in memory while Syrtis runs (U3), dropped (erased by
// `Key`'s Drop) when the "cursor" grant is removed or sync is turned off. The
// Keychain is read at most once per round, and only by a round the user
// started (`explicit`) or the process's first round: a later background round
// never raises the dialog.

struct KeyCache {
    key: Option<cursor_desktop::Key>,
    /// Bumped by every drop. A read stores its key only if no drop happened
    /// while it waited on the dialog.
    generation: u64,
}

static SEALED_KEY: Mutex<KeyCache> = Mutex::new(KeyCache {
    key: None,
    generation: 0,
});

/// Set when the first `sync_with` round of the process ends, whatever it
/// found (plaintext, no token, a refusal), so only that round may read
/// without the user asking.
static FIRST_ROUND_DONE: AtomicBool = AtomicBool::new(false);

fn sealed_key() -> std::sync::MutexGuard<'static, KeyCache> {
    SEALED_KEY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Drop the cached key, if any, and invalidate a read in flight. Called with
/// no other registry lock held (`keychain_consent::set_from_json`,
/// `set_from_json` here, a round in which the key opened no token).
pub(crate) fn drop_sealed_key() {
    let mut cache = sealed_key();
    cache.key = None;
    cache.generation = cache.generation.wrapping_add(1);
}

fn read_key(fetch: &KeyFetch<'_>) -> Result<cursor_desktop::Key, &'static str> {
    #[cfg(target_os = "macos")]
    {
        use crate::macos_safe_storage::{read_cursor_key, KeychainError};
        read_cursor_key(fetch).map_err(|error| match error {
            KeychainError::Denied => "keychain_denied",
            KeychainError::NotFound => "keychain_missing",
            KeychainError::Other => "keychain_unavailable",
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = fetch;
        Err("token_sealed_unsupported")
    }
}

/// One round's sealed-token state.
struct SealedRound<'a> {
    /// The round may still read the Keychain: explicit or the process's
    /// first round, and no read yet this round.
    may_read: bool,
    /// The global sync switch (the round's live config), as opposed to one
    /// profile's `enabled`: a key read is kept only while it is on.
    switch_on: &'a dyn Fn() -> bool,
    /// The round's one read failed; later profiles reuse the reason (never a
    /// second dialog).
    failed: Option<&'static str>,
    /// Some profile tried the key; some profile's token opened with it.
    tried: bool,
    opened: bool,
}

impl<'a> SealedRound<'a> {
    fn new(may_read: bool, switch_on: &'a dyn Fn() -> bool) -> Self {
        Self {
            may_read,
            switch_on,
            failed: None,
            tried: false,
            opened: false,
        }
    }

    /// The plaintext of one profile's sealed token, or the fixed refusal.
    /// `enabled`: the profile's live switch (still configured as when the
    /// round started).
    fn open(
        &mut self,
        blob: &[u8],
        target: &Target<'_>,
        enabled: &dyn Fn() -> bool,
    ) -> Result<String, Stop> {
        let refuse = |reason| Err(Stop::error(reason));
        let Some(fetch) = target.key_fetch else {
            return refuse("token_sealed_unsupported");
        };
        let mut cache = sealed_key();
        // Checked under the key-cache lock, never before it: a revoke writes
        // the registry first and then drops the key under this lock, so the
        // answer read here holds until the decrypt below is done.
        if !(target.consent)() {
            return refuse("keychain_consent");
        }
        if cache.key.is_none() {
            if let Some(reason) = self.failed {
                return refuse(reason);
            }
            if !self.may_read {
                return refuse("keychain_unlock");
            }
            self.may_read = false;
            let generation = cache.generation;
            // Not held across the read: it blocks until the dialog is
            // answered, and a drop must not wait for that.
            drop(cache);
            let key = match read_key(fetch) {
                Ok(key) => key,
                Err(reason) => {
                    self.failed = Some(reason);
                    return refuse(reason);
                }
            };
            cache = sealed_key();
            // Stored only if no drop happened while the dialog was up (an off
            // push or a consent payload without "cursor" bumps the
            // generation), consent stands and sync is still on; otherwise
            // `key` is dropped (erased) here.
            if !(self.switch_on)() {
                return Err(Stop::new(State::Disabled, None));
            }
            if cache.generation != generation || !(target.consent)() {
                return refuse("keychain_consent");
            }
            cache.key = Some(key);
            // A profile removed while the dialog was up only skips itself:
            // the key the user just allowed stays for the other profiles.
            if !enabled() {
                return Err(Stop::new(State::Disabled, None));
            }
        }
        let Some(key) = cache.key.as_ref() else {
            return refuse("keychain_unlock");
        };
        self.tried = true;
        let token = cursor_desktop::unseal(blob, key).map_err(Stop::error)?;
        self.opened = true;
        Ok(token)
    }

    /// A key no profile of the round could open its token with is dropped
    /// (a rotated Safe Storage key: the next Sync Now reads again). One bad
    /// profile beside a good one keeps it.
    fn finish(self) {
        if self.tried && !self.opened {
            drop_sealed_key();
        }
    }
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
    let claims = cursor_desktop::jwt_claims(&login.access_token).ok_or(unreadable)?;
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

pub(crate) enum BodyError {
    TooLarge,
    Timeout,
    Transport,
}

pub(crate) async fn read_capped(
    response: &mut reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, BodyError> {
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

/// A send that failed: a timeout is `partial`, anything else `offline`.
pub(crate) fn send_stop(error: &reqwest::Error) -> Stop {
    if error.is_timeout() {
        Stop::new(State::Partial, Some("timeout"))
    } else {
        Stop::new(State::Offline, Some("unreachable"))
    }
}

pub(crate) fn body_stop(error: BodyError) -> Stop {
    match error {
        BodyError::TooLarge => Stop::error("body_too_large"),
        BodyError::Timeout => Stop::new(State::Partial, Some("timeout")),
        BodyError::Transport => Stop::new(State::Offline, Some("unreachable")),
    }
}

/// The stop for a response that is not a 2xx, or None for a 2xx. Shared by
/// both walks, so V2 answers each status the way the old walk does.
pub(crate) async fn refusal(response: &mut reqwest::Response) -> Option<Stop> {
    let status = response.status();
    if status.is_redirection() || status.as_u16() == 401 {
        return Some(Stop::new(State::Expired, None));
    }
    if status.as_u16() == 403 {
        // P3-4: an auth refusal is JSON; an HTML wall (bot check, outage
        // page) is not a sign-in problem and must not tell the user to
        // sign in again.
        let json = read_capped(response, MAX_ERROR_BODY_BYTES)
            .await
            .ok()
            .is_some_and(|b| serde_json::from_slice::<serde_json::Value>(&b).is_ok());
        return Some(if json {
            Stop::new(State::Expired, None)
        } else {
            Stop::error("forbidden_non_json")
        });
    }
    if status.as_u16() == 429 {
        // A maintainer stop condition (plan C2 stops), distinct on purpose.
        return Some(Stop::error("rate_limited"));
    }
    if status.is_server_error() {
        return Some(Stop::error("server_error"));
    }
    if !status.is_success() {
        return Some(Stop::error("http_status"));
    }
    None
}

/// D1a: a JSON object with no `usageEventsDisplay` key (only the key names
/// are kept, in memory). Anything else, a malformed array included, is not.
fn lacks_usage_events(body: &[u8]) -> bool {
    serde_json::from_slice::<std::collections::BTreeMap<String, serde::de::IgnoredAny>>(body)
        .is_ok_and(|keys| !keys.contains_key("usageEventsDisplay"))
}

/// The client both walks use: no redirects, https only in production.
fn client(target: &Target<'_>) -> Result<reqwest::Client, Stop> {
    crate::agent_usage::provider_http_client_builder()
        .redirect(reqwest::redirect::Policy::none())
        .https_only(target.https_only)
        .build()
        .map_err(|_| Stop::error("client_unavailable"))
}

/// The old endpoint's complete history, or None when its page 1 is a 2xx
/// JSON object without `usageEventsDisplay` (a Billing V2 account, D1a).
async fn walk(
    client: &reqwest::Client,
    target: &Target<'_>,
    cookie: &reqwest::header::HeaderValue,
    limits: Limits,
    enabled: &dyn Fn() -> bool,
) -> Result<Option<Vec<Event>>, Stop> {
    use reqwest::header;
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
        let mut response = sent.map_err(|error| send_stop(&error))?;
        if let Some(stop) = refusal(&mut response).await {
            return Err(stop);
        }
        let cap = limits.max_body_bytes.saturating_sub(bytes_read);
        let body = read_capped(&mut response, cap).await.map_err(body_stop)?;
        bytes_read += body.len();
        if page == 1 && lacks_usage_events(&body) {
            return Ok(None);
        }
        let page: Page =
            serde_json::from_slice(&body).map_err(|_| Stop::error("unexpected_response"))?;
        let received = page.usage_events_display.len();
        events.extend(page.usage_events_display);
        let reached_total = page
            .total_usage_events_count
            .is_some_and(|total| events.len() as u64 >= total);
        if received < limits.page_size as usize || reached_total {
            return Ok(Some(events));
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
/// that is not this session's own (see the module doc). An added profile
/// (`slot.root` set) is read with `read_login_checked` (a symlinked database
/// refused); the default profile follows a symlinked `state.vscdb`, as every
/// release did, and opens the resolved file the checked way without an owner
/// check (`read_login_resolved`). `sealed`: the round's sealed-token state.
async fn run(
    target: &Target<'_>,
    slot: &Slot,
    limits: Limits,
    enabled: &dyn Fn() -> bool,
    claim: &mut dyn FnMut(&str, bool) -> Option<usize>,
    sealed: &mut SealedRound<'_>,
) -> Outcome {
    let stopped = Outcome::stopped;
    // `limits.budget` is what was left of the round's budget when this
    // profile started; time spent waiting on the Keychain dialog comes off it.
    let started = Instant::now();
    let dir = slot.dir.as_path();
    if !enabled() {
        return stopped(Stop::new(State::Disabled, None));
    }
    let read = if slot.root.is_some() {
        cursor_desktop::read_login_checked(&slot.db)
    } else {
        cursor_desktop::read_login_resolved(&slot.db)
    };
    let mut login = match read {
        Ok(Some(login)) => login,
        Ok(None) => return stopped(Stop::new(State::NotSignedIn, None)),
        Err(reason) => return Outcome::refused(Stop::error(reason)),
    };
    // A sealed token is opened before anything reads it; a plaintext one is
    // used as stored, with no consent lookup and no key (Design 1, 3).
    match cursor_desktop::stored_token(&login.access_token) {
        cursor_desktop::StoredToken::Plain => {}
        cursor_desktop::StoredToken::Sealed(blob) => match sealed.open(&blob, target, enabled) {
            Ok(token) => login.access_token = token,
            Err(stop) if stop.state == State::Disabled => return stopped(stop),
            Err(stop) => return Outcome::refused(stop),
        },
        cursor_desktop::StoredToken::Refused(reason) => {
            return Outcome::refused(Stop::error(reason))
        }
    }
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
        // The account's own Billing V2 sidecar stays with its live file.
        let cleaned = locked_removal(dir, &[&live_name(&stem), &sidecar_name(&stem)], false);
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
    let limits = Limits {
        budget: limits.budget.saturating_sub(started.elapsed()),
        ..limits
    };
    // D5: a same-stem sidecar Syrtis cannot read (a non-file, or not ours
    // to parse) stops the round before any request, old or V2.
    if read_history(dir, &stem).is_err() {
        return stopped(Stop::error("write_failed"));
    }
    let client = match client(target) {
        Ok(client) => client,
        Err(stop) => return stopped(stop),
    };
    let walk_started = Instant::now();
    let events = match walk(&client, target, &cookie, limits, enabled).await {
        Ok(Some(events)) => events,
        Ok(None) => {
            let limits = Limits {
                budget: limits.budget.saturating_sub(walk_started.elapsed()),
                ..limits
            };
            let (synced, dropped) =
                billing_v2(&client, target, dir, &stem, &user, &cookie, limits, enabled).await;
            drop(cookie);
            return match synced {
                Ok((committed, events)) => {
                    claim(&stem, true);
                    Outcome {
                        events,
                        changed: committed.changed,
                        cleanup_failed: committed.cleanup_failed,
                        dropped,
                        ..stopped(Stop::new(State::Ok, None))
                    }
                }
                Err(stop) => {
                    if covers_account(stop) {
                        claim(&stem, true);
                    }
                    Outcome {
                        dropped,
                        ..stopped(stop)
                    }
                }
            };
        }
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

/// D1a's other half: the V2 walk over this round's months, then the V2
/// commit, with the count of rows the walk dropped (`Outcome::dropped`). A
/// refusal or partial walk returns before the commit, so the live file and
/// the sidecar stay as they were. Any failure here covers the account like
/// a walk failure (`covers_account`): write_failed and disabled do not.
#[allow(clippy::too_many_arguments)]
async fn billing_v2(
    client: &reqwest::Client,
    target: &Target<'_>,
    dir: &Path,
    stem: &str,
    user: &str,
    cookie: &reqwest::header::HeaderValue,
    limits: Limits,
    enabled: &dyn Fn() -> bool,
) -> (Result<(Committed, usize), Stop>, usize) {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    // A live file (or a sidecar, since `run` checked it) the commit could
    // not read fails here, before any V2 request.
    let (Ok(stored), Ok(live)) = (read_history(dir, stem), read_live(dir, stem)) else {
        return (Err(Stop::error("write_failed")), 0);
    };
    let stale = stored
        .as_ref()
        .is_some_and(|history| !history.trusts(&live));
    let history = stored.filter(|_| !stale);
    let plan = v2_windows(history.as_ref(), &live.rows, stale, limits.explicit, now_ms);
    drop((history, live));
    let fetched = match cursor_billing_v2::walk(
        client,
        target.billing_url,
        cookie,
        user,
        limits,
        enabled,
        &plan.windows,
        plan.stop_after_empty,
        now_ms,
    )
    .await
    {
        Ok(fetched) => fetched,
        Err(stop) => return (Err(stop), 0),
    };
    let dropped = fetched.dropped_after(plan.cutover);
    (commit_v2(dir, stem, fetched, now_ms, enabled), dropped)
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
    #[cfg(target_os = "macos")]
    let key_fetch: Option<&KeyFetch<'_>> = Some(&crate::macos_safe_storage::cursor_keychain_fetch);
    #[cfg(not(target_os = "macos"))]
    let key_fetch: Option<&KeyFetch<'_>> = None;
    sync_with(
        explicit,
        home,
        &Target {
            db_path: &db_path,
            url: USAGE_EVENTS_URL,
            billing_url: cursor_billing_v2::BILLING_ITEMS_URL,
            https_only: true,
            file_stem: &stem,
            key_fetch,
            consent: &|| crate::keychain_consent::allowed("cursor"),
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
    let switch_on = || config().enabled;
    let mut sealed = SealedRound::new(
        explicit || !FIRST_ROUND_DONE.load(Ordering::SeqCst),
        &switch_on,
    );
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
                profile,
                Limits {
                    budget: remaining,
                    explicit,
                    ..limits
                },
                &still_wanted,
                &mut claim,
                &mut sealed,
            ))
        };
        outcomes.push(outcome);
    }
    sealed.finish();
    FIRST_ROUND_DONE.store(true, Ordering::SeqCst);
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
            if outcome.dropped > 0 {
                entry["droppedRows"] = outcome.dropped.into();
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

/// Test reset seam for the sealed-token state (the cached key, its
/// generation, `FIRST_ROUND_DONE`). Callers hold `TEST_LOCK`.
#[cfg(test)]
pub(crate) fn reset_sealed_for_test() {
    drop_sealed_key();
    FIRST_ROUND_DONE.store(false, Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) fn sealed_key_cached_for_test() -> bool {
    sealed_key().key.is_some()
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
