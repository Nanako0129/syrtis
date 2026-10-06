//! Cursor usage synced from the signed-in Cursor desktop app.
//!
//! Data flow: Cursor's `state.vscdb` (read-only, `cursor_desktop`) → access
//! token + its own JWT `sub` → POST `get-filtered-usage-events` on
//! cursor.com, full history, paginated → a trimmed JSON file
//! `usage.<hmac>.json` in a Syrtis-owned sync dir → the engine scans that dir
//! as an extra Cursor root (`apply_takeover`). Aggregation happens in the
//! engine; this module only produces the file and the scan settings.
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Config {
    pub enabled: bool,
    pub dir: Option<PathBuf>,
    pub cli_takeover_confirmed: bool,
}

static CONFIG: LazyLock<RwLock<Config>> = LazyLock::new(|| RwLock::new(Config::default()));

pub(crate) fn config() -> Config {
    CONFIG
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SetInput {
    enabled: bool,
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    cli_takeover_confirmed: bool,
}

/// `<HOME>/.config/tokscale/cursor-cache`, the CLI's Cursor root
/// (engine `clients.rs`, `PathRoot::Home`).
fn cli_root(home: &Path) -> PathBuf {
    home.join(".config/tokscale/cursor-cache")
}

fn validate_dir(raw: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    let dir = PathBuf::from(raw);
    if !dir.is_absolute() || dir.parent().is_none() {
        return Err("cursor sync dir must be an absolute, non-root path".to_string());
    }
    if dir.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("cursor sync dir must not contain '..'".to_string());
    }
    // D1: Syrtis never writes into the tokscale CLI's tree, and a sync dir
    // under the CLI root would be dropped by the takeover's own exclusion.
    if let Some(home) = home {
        if dir.starts_with(home.join(".config/tokscale")) {
            return Err("cursor sync dir must not be inside ~/.config/tokscale".to_string());
        }
    }
    Ok(dir)
}

/// Replace the registry from `{"enabled":bool,"dir":"/abs","cliTakeoverConfirmed":bool}`.
/// `dir` is required while enabled. Turning sync off, or moving the dir,
/// deletes the Syrtis usage files from the dir no longer in use (P3-7).
/// Invalid input leaves the registry untouched.
pub(crate) fn set_from_json(raw: &str, home: Option<&Path>) -> Result<serde_json::Value, String> {
    let input: SetInput =
        serde_json::from_str(raw).map_err(|_| "invalid cursor sync JSON".to_string())?;
    let dir = input
        .dir
        .as_deref()
        .map(|raw| validate_dir(raw, home))
        .transpose()?;
    if input.enabled && dir.is_none() {
        return Err("cursor sync dir is required when enabled".to_string());
    }
    let new = Config {
        enabled: input.enabled,
        dir,
        cli_takeover_confirmed: input.cli_takeover_confirmed,
    };
    let old = {
        let mut guard = CONFIG
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::replace(&mut *guard, new.clone())
    };
    let mut removed = 0;
    if let Some(old_dir) = old.dir.as_deref() {
        if !new.enabled || new.dir.as_deref() != Some(old_dir) {
            removed += remove_usage_files_locked(old_dir);
        }
    }
    if let Some(new_dir) = new.dir.as_deref() {
        if !new.enabled && old.dir.as_deref() != Some(new_dir) {
            removed += remove_usage_files_locked(new_dir);
        }
    }
    Ok(serde_json::json!({
        "enabled": new.enabled,
        "dir": new.dir.as_deref().map(|d| d.to_string_lossy()),
        "cliTakeoverConfirmed": new.cli_takeover_confirmed,
        "removedFiles": removed,
    }))
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
        dir: PathBuf,
    },
}

pub(crate) fn takeover(config: &Config, home: Option<&Path>) -> Takeover {
    let (true, Some(dir), Some(home)) = (config.enabled, config.dir.as_deref(), home) else {
        return Takeover::Off;
    };
    if complete_file(dir).is_none() {
        return Takeover::Off;
    }
    let cli_root = cli_root(home);
    if cli_has_cursor_files(&cli_root) && !config.cli_takeover_confirmed {
        return Takeover::Blocked;
    }
    Takeover::On {
        cli_root,
        dir: dir.to_path_buf(),
    }
}

/// Merge the takeover into settings already holding the registry's extra
/// paths (e.g. Claude's), never replacing them.
pub(crate) fn apply_takeover(
    settings: &mut tokscale_core::scanner::ScannerSettings,
    config: &Config,
    home: Option<&Path>,
) {
    if let Takeover::On { cli_root, dir } = takeover(config, home) {
        settings
            .extra_scan_paths
            .entry("cursor".to_string())
            .or_default()
            .push(dir);
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

/// The newest complete synced file in `dir`, with its mtime. Only complete
/// walks ever create one, so existence is completeness.
fn complete_file(dir: &Path) -> Option<(PathBuf, SystemTime)> {
    if !std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        return None;
    }
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|entry| is_sync_file_name(&entry.file_name().to_string_lossy()))
        .filter_map(|entry| {
            let meta = std::fs::symlink_metadata(entry.path()).ok()?;
            let modified = meta.modified().ok()?;
            meta.is_file().then(|| (entry.path(), modified))
        })
        .max_by_key(|(_, modified)| *modified)
}

// ---------------------------------------------------------------------------
// Files.

/// Delete Syrtis usage files (and temp leftovers) in `dir` except `keep`.
/// Never touches anything else. Caller holds the dir lock.
fn remove_usage_files(dir: &Path, keep: Option<&str>) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let ours = (name.starts_with("usage.") && name.ends_with(".json"))
                || name.starts_with(TEMP_FILE_PREFIX);
            ours && Some(name.as_ref()) != keep
        })
        .filter(|entry| std::fs::remove_file(entry.path()).is_ok())
        .count()
}

fn remove_usage_files_locked(dir: &Path) -> usize {
    // Only a real directory: never follow a symlink to delete elsewhere.
    if !std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        return 0;
    }
    let Ok(lock) = crate::agent_quota_history::open_owner_only(&dir.join(LOCK_FILE_NAME)) else {
        return 0;
    };
    if lock.lock_exclusive().is_err() {
        return 0;
    }
    remove_usage_files(dir, None)
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
    remove_usage_files(dir, Some(&final_name));
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

/// One sync, start to finish. `enabled` is the live switch (and, in
/// production, "the dir is still the one this walk started with").
async fn run(
    target: &Target<'_>,
    dir: &Path,
    limits: Limits,
    enabled: &dyn Fn() -> bool,
) -> Outcome {
    let stopped = |stop| Outcome { stop, events: 0 };
    if !enabled() {
        return stopped(Stop::new(State::Disabled, None));
    }
    let login = match cursor_desktop::read_login(target.db_path) {
        Ok(Some(login)) => login,
        Ok(None) => return stopped(Stop::new(State::NotSignedIn, None)),
        Err(_) => return stopped(Stop::error("login_unreadable")),
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
    match commit_file(dir, &stem, &events, enabled) {
        Ok(()) => Outcome {
            stop: Stop::new(State::Ok, None),
            events: events.len(),
        },
        Err(stop) => stopped(stop),
    }
}

// ---------------------------------------------------------------------------
// Entry point (single-flight).

/// The last status, behind the single-flight lock. A caller arriving while a
/// sync runs waits for it and returns its status instead of starting another.
static IN_FLIGHT: Mutex<Option<serde_json::Value>> = Mutex::new(None);

/// Run one sync against Cursor. Returns the status JSON and whether the
/// synced file changed (the caller then invalidates scan caches).
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
    let current = config();
    let outcome = match (current.enabled, current.dir.clone()) {
        (true, Some(dir)) => {
            let budget = if explicit || complete_file(&dir).is_none() {
                FULL_BUDGET
            } else {
                BACKGROUND_BUDGET
            };
            let limits = test_limits.unwrap_or(Limits::production(budget));
            let still_wanted = || {
                let now = config();
                now.enabled && now.dir.as_deref() == Some(dir.as_path())
            };
            let outcome = crate::RUNTIME.block_on(run(target, &dir, limits, &still_wanted));
            (outcome, Some(dir))
        }
        _ => (
            Outcome {
                stop: Stop::new(State::Disabled, None),
                events: 0,
            },
            None,
        ),
    };
    let (outcome, dir) = outcome;
    let changed = outcome.stop.state == State::Ok;
    let state = if changed && takeover(&config(), home) == Takeover::Blocked {
        State::CliPresent
    } else {
        outcome.stop.state
    };
    let last_success_ms = dir
        .as_deref()
        .and_then(complete_file)
        .and_then(|(_, modified)| modified.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    let mut status = serde_json::json!({
        "state": state,
        "events": outcome.events,
        "lastSuccessMs": last_success_ms,
    });
    if let Some(reason) = outcome.stop.reason {
        status["reason"] = reason.into();
    }
    *slot = Some(status.clone());
    (status, changed)
}

#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn set_for_test(config: Config) {
    *CONFIG.write().unwrap_or_else(|p| p.into_inner()) = config;
}

#[cfg(test)]
mod tests;
