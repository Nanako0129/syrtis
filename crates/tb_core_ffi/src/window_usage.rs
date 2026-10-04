//! Window usage for the quota lens: per-message rows inside an absolute interval.
//!
//! Returns the messages inside an absolute [from, until) window, one row each.
//! No bucketing: a quota window is a tiny slice of history, so the consumer
//! folds it however the UI wants without another round trip. Attribution is a
//! Swift-side declaration, so it is deliberately NOT applied here.
//!
//! **Scoped to one account.** A quota window belongs to one account, so the
//! usage divided against it has to come from that account's transcripts and no
//! others. With a second Claude account configured the roots are disjoint on
//! disk, so the scope is applied by narrowing the scan rather than by tagging
//! each message with an owner — see [`account_options`]. That keeps the wire
//! model, and therefore the source-message cache's bincode layout, untouched.

use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// `None` is the primary account. An extra Claude account is keyed by its
/// `CLAUDE_CONFIG_DIR`, the same string its card and its quota curve are keyed
/// on (`AgentUsageSnapshot::account_key`).
///
/// Only Claude accounts reach this. A captured Antigravity account also has an
/// `account_key` (a 64-hex key, not a path), but Antigravity has no local logs
/// to scan, so `DashboardModel.refreshWindowUsage` drops those accounts before
/// it builds a request. A key passed anyway matches no registered root and
/// gets `NO_REGISTERED_ROOTS`.
pub(crate) type Account = Option<String>;
pub(crate) type CacheKey = (Account, i64, i64);
pub(crate) type CacheEntry = (Instant, u64, Value);

const MINUTE_MS: i64 = 60_000;
const CLAUDE: &str = "claude";
static SCAN_COUNT: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn cache_key(account: &Account, from_ms: i64, until_ms: i64) -> CacheKey {
    // Saturation keeps the extreme negative i64 input from overflowing while
    // preserving the minute floor for normal timestamps.
    (
        account.clone(),
        from_ms,
        until_ms.saturating_sub(until_ms.rem_euclid(MINUTE_MS)),
    )
}

/// The scan roots the extra config directory `dir` owns, taken from the
/// registry the scanner actually walks rather than re-derived here.
///
/// Intersecting with the registry rather than rebuilding `<dir>/projects` and
/// `<dir>/transcripts` locally keeps this honest about paths the setter
/// refused: a root that is not registered is not scanned for anybody, and
/// listing it here would claim otherwise.
///
/// **Direct children only, not every descendant.** Nothing stops a user
/// configuring both `/work` and `/work/sub` as accounts — `isRejectedRoot`
/// refuses only the empty path, `/` and the home directory
/// (`ClaudeExtraRoots.swift:53`) — and a prefix test would then hand `/work`
/// the roots of `/work/sub`, folding one account's transcripts into the
/// other's allowance. That is the defect this file exists to fix, reappearing
/// on the selection side after being fixed on the exclusion side.
///
/// A parent comparison rather than a rebuilt `{projects, transcripts}` set,
/// because every registry entry is written by `ClaudeExtraRoots.expand` and is
/// a direct child of the directory it belongs to; restating that list here
/// would be a second copy of it that could drift. The coupling is to the
/// *shape* of the expansion, not to its contents.
///
/// Note the asymmetry with `primary_exclusions`, which is deliberate: the
/// exclusion wants every descendant, because anything under a configured
/// directory is not the primary's. The selection wants exactly this account's,
/// because a nested account is somebody else's.
fn registered_roots_under(dir: &str) -> Vec<PathBuf> {
    let owner = PathBuf::from(dir);
    crate::extra_scan_paths::snapshot()
        .get(CLAUDE)
        .map(|paths| {
            paths
                .iter()
                .filter(|path| path.parent() == Some(owner.as_path()))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Every path the primary scan must not read: the configured extra config
/// directories, and the scan roots registered for them.
///
/// The union is not a spare guard — each half catches an account the other
/// misses, and the two come from registries installed by separate FFI calls
/// (`tb_set_claude_config_dirs` and `tb_set_extra_scan_paths`) with separate
/// rejection rules, so neither implies the other:
///
/// - A **config directory** catches roots under it that are not registry
///   entries. `$HOME/.cc-mirror/*/variant.json` may name any absolute
///   directory, and the scanner then reads `<that dir>/projects` — including a
///   directory nested inside a configured one, which no registry entry is a
///   prefix of.
/// - A **registered root** catches an account whose directory reached the scan
///   registry but not the config-dir registry. `ClaudeExtraRoots.install`
///   makes the two calls separately and discards the first's result, so the
///   lists can disagree, and the disagreement is silent.
///
/// Excluding neither is how the extra account's transcripts land on the primary
/// card, which is what issue #258 reports.
fn primary_exclusions() -> Vec<PathBuf> {
    let mut excluded: Vec<PathBuf> = crate::claude_config_dirs::snapshot()
        .into_iter()
        .map(PathBuf::from)
        .collect();
    if let Some(roots) = crate::extra_scan_paths::snapshot().get(CLAUDE) {
        excluded.extend(roots.iter().cloned());
    }
    excluded
}

/// The other accounts nested under `dir`, which `dir`'s own window must not
/// read: their configured directories, and every registered Claude root under
/// `dir` that is not one of `dir`'s own.
///
/// An extra account's window is scanned with `dir` as home, and the engine
/// adds `<dir>/.claude/projects`, `<dir>/.claude/transcripts` and the cowork
/// trees under it on its own. So an account configured at `dir/.claude` was
/// counted both here and in its own window. Only paths under `dir` are listed:
/// an unrelated account is never reached from this home, and an account that
/// contains this one (an ancestor) must not be listed, or the exclusion would
/// remove this account's own files. Compared component-wise as given; a nested
/// account registered under another spelling of its folder (a symlink, a
/// different case on a case-insensitive volume) is not recognised and stays
/// counted twice, as before, rather than hidden. Windows does the same
/// (Syrtis-Windows #203).
fn nested_exclusions(dir: &str, own: &[PathBuf]) -> Vec<PathBuf> {
    let owner = Path::new(dir);
    let nested = |path: &Path| path != owner && path.starts_with(owner);
    let mut excluded: Vec<PathBuf> = crate::claude_config_dirs::snapshot()
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| nested(path))
        .collect();
    if let Some(roots) = crate::extra_scan_paths::snapshot().get(CLAUDE) {
        excluded.extend(
            roots
                .iter()
                .filter(|root| !own.contains(root) && nested(root))
                .cloned(),
        );
    }
    // The engine canonicalises an exclusion before matching it, so one that is
    // nested only as spelled (`D/.claude` a symlink back to `D` or above)
    // would cover this account's own roots and empty its window. Such an
    // entry is dropped: counted twice beats confidently zero.
    let canonical = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let own: Vec<PathBuf> = own.iter().map(|root| canonical(root)).collect();
    excluded.retain(|path| {
        let path = canonical(path);
        !own.iter().any(|root| root.starts_with(&path))
    });
    excluded
}

/// Report options that read one account's transcripts and nothing else.
///
/// The primary keeps the real home and every client, and simply refuses the
/// configured extra accounts. An extra account is rooted at its own config
/// directory instead, where no built-in root resolves, so only its registered
/// roots contribute.
///
/// `use_env_roots` is false for an extra account for one narrow reason worth
/// stating exactly, because it is easy to overclaim: Claude's declared root is
/// `PathRoot::Home` and the flag does not gate it. What the flag gates is
/// `TOKSCALE_EXTRA_DIRS`, which can name `claude:` roots of its own and would
/// otherwise pour another account's files into this one's scan.
///
/// ponytail: a message present under two accounts' roots is counted for both,
/// because it is genuinely present in both and carries nothing that says which
/// account produced it. Measured at 2 messages in 10,389 on the reporting
/// machine. Upgrade path is an owner on the message, which costs a
/// `CACHE_FORMAT_VERSION` bump and a cold re-parse for every user.
///
/// One asymmetry this deliberately accepts: `UsageAttribution` lets a user
/// declare another client's usage against the Claude subscription, and those
/// rows are present in the primary's scan and absent from an extra account's.
/// A declaration names a client, a provider and a model and carries no account,
/// so there is no answer to which of two Claude accounts it belonged to;
/// counting it once, for the primary, beats today's counting it for both.
/// An extra account with no registered scan root cannot be answered for, and
/// says so instead of reporting an empty window.
///
/// The two registries are installed by separate FFI calls and
/// `ClaudeExtraRoots.install` deliberately wakes the quota pollers between them
/// — a stalled mount would otherwise hold the cards on the previous account set
/// for the whole filesystem timeout. So a just-added account is briefly present
/// in `claude_config_dirs` and absent from `extra_scan_paths`, and an account
/// whose roots the setter refused outright is in that state permanently.
///
/// Reporting zero rows there would be the same class of mistake this file
/// exists to fix, in the other direction: an empty scan means "nothing was
/// read", and rendering it as a window means "this account used nothing". A
/// failed read leaves the previous estimate standing, which is stale but was
/// once true; a zero is confidently wrong.
///
/// Deliberately not fixed by making the two registries atomic. They are two
/// FFI calls, so no reader-side snapshot can span them, and making the wake
/// wait for both reintroduces exactly the stall that ordering was chosen to
/// remove (`ClaudeExtraRoots.install`).
const NO_REGISTERED_ROOTS: &str =
    "This Claude account has no registered scan root yet, so its window cannot be read.";

fn account_options(
    context: &crate::LocalSourceContext,
    account: &Account,
) -> Result<tokscale_core::ReportOptions, String> {
    Ok(match account {
        None => {
            let mut options = context.report_options(None, None);
            let excluded = primary_exclusions();
            if !excluded.is_empty() {
                options
                    .scanner_settings
                    .excluded_scan_paths
                    .insert(CLAUDE.to_string(), excluded);
            }
            options
        }
        Some(dir) => {
            let roots = registered_roots_under(dir);
            if roots.is_empty() {
                return Err(NO_REGISTERED_ROOTS.to_string());
            }
            let excluded = nested_exclusions(dir, &roots);
            let mut scanner_settings = tokscale_core::scanner::ScannerSettings {
                extra_scan_paths: BTreeMap::from([(CLAUDE.to_string(), roots)]),
                ..Default::default()
            };
            if !excluded.is_empty() {
                scanner_settings
                    .excluded_scan_paths
                    .insert(CLAUDE.to_string(), excluded);
            }
            tokscale_core::ReportOptions {
                home_dir: Some(dir.clone()),
                use_env_roots: false,
                // Exactly the Claude client. Rooted at `dir`, the engine also
                // resolves every other client's home routes under it, and a
                // `.cc-mirror` variant there may name a directory anywhere;
                // those rows carry `cc-mirror/<variant>` or another client id
                // and could reach this account's card through a usage
                // attribution declaring them against Claude. The engine
                // matches this per message (`report_message_client_passes`).
                clients: Some(vec![CLAUDE.to_string()]),
                scanner_settings,
                ..Default::default()
            }
        }
    })
}

/// The change token that decides whether a cached window is still good, read
/// through the same account scope the window itself was scanned under.
///
/// Sharing one token across accounts would make either account's files moving
/// invalidate the other's entry, which is the cache thrash this account-keyed
/// map exists to avoid.
fn account_parse_options(
    context: &crate::LocalSourceContext,
    account: &Account,
) -> Result<tokscale_core::LocalParseOptions, String> {
    let report = account_options(context, account)?;
    Ok(tokscale_core::LocalParseOptions {
        home_dir: report.home_dir,
        use_env_roots: report.use_env_roots,
        clients: report.clients,
        scanner_settings: report.scanner_settings,
        ..Default::default()
    })
}

/// Test instrumentation: the counter exists in every build so `cached` can
/// increment it, but reading it is only ever a test's business.
#[cfg(test)]
pub(crate) fn scan_count() -> usize {
    SCAN_COUNT.load(Ordering::Relaxed)
}

pub(crate) fn cached(
    context: &crate::LocalSourceContext,
    account: &Account,
    from_ms: i64,
    until_ms: i64,
) -> Result<Value, String> {
    let key = cache_key(account, from_ms, until_ms);
    let cached = {
        let cache = crate::WINDOW_USAGE_CACHE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.get(&key).map(|(at, token, data)| {
            (
                at.elapsed() <= Duration::from_secs(crate::ONESHOT_MAX_AGE_SECS),
                *token,
                data.clone(),
            )
        })
    };
    let Some((fresh_enough, token, data)) = cached else {
        return compute(context, account, from_ms, until_ms, key);
    };
    if fresh_enough {
        return Ok(data);
    }

    // Probe with the cache lock released, matching graph_cached. An unchanged
    // source only refreshes the timestamp; it does not re-run the scan.
    if let Ok(probe_token) = account_parse_options(context, account)
        .and_then(|options| tokscale_core::local_source_change_token(&options))
    {
        if probe_token == token {
            let mut cache = crate::WINDOW_USAGE_CACHE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(entry) = cache.get_mut(&key) {
                entry.0 = Instant::now();
            }
            return Ok(data);
        }
    }

    compute(context, account, from_ms, until_ms, key)
}

/// Held across a scan so overlapping callers share one, instead of each
/// starting its own.
///
/// `PopoverView`'s keyed window task and `pollAgentUsage` can both reach
/// `refreshWindowUsage`, and the Swift scan token only discards an overtaken
/// RESULT — it does not stop the duplicate work. Two 4-to-67 second scans then
/// run at once and contend for the same parser pool, which is the opposite of
/// what a cache in front of them is for.
///
/// A plain mutex rather than a per-key future: the second caller waits, then
/// re-checks the cache and finds what the first published. Scans are CPU-bound
/// and already serialise on the parser pool, so making them queue costs nothing
/// that running them concurrently was buying.
static COMPUTE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn compute(
    context: &crate::LocalSourceContext,
    account: &Account,
    from_ms: i64,
    until_ms: i64,
    key: CacheKey,
) -> Result<Value, String> {
    let _serialised = COMPUTE.lock().unwrap_or_else(|p| p.into_inner());
    // Re-check under the lock. A caller that queued behind another's scan is
    // asking a question that scan may have just answered; running a second one
    // to produce the same bytes is the duplicate this lock exists to remove.
    {
        let cache = crate::WINDOW_USAGE_CACHE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((at, _, data)) = cache.get(&key) {
            if at.elapsed() <= Duration::from_secs(crate::ONESHOT_MAX_AGE_SECS) {
                return Ok(data.clone());
            }
        }
    }
    let generation = crate::root_generation();
    let token = account_parse_options(context, account)
        .and_then(|options| tokscale_core::local_source_change_token(&options))
        .unwrap_or(0);
    let data = run(context, account, from_ms, until_ms)?;
    // The caller still gets this payload when `publish` refuses it: it answers
    // a question asked before the roots changed, and the next call recomputes.
    publish(key, generation, (Instant::now(), token, data.clone()));
    Ok(data)
}

/// Cache a freshly scanned window unless the root registry moved while the scan
/// was running.
///
/// Same lock discipline as `publish_graph`: the generation is re-read inside
/// the lock `invalidate_scan_caches` clears under, so a replace cannot land
/// between the check and the insert. A window scan runs for tens of seconds on
/// a large store, which makes that interleaving ordinary rather than exotic
/// here — and the entry it would leave behind is served without a token probe
/// for `ONESHOT_MAX_AGE_SECS`.
///
/// The insert clears first: one entry per account, not a history. The key
/// carries `until_ms` floored to the minute and the consumer polls every 60s
/// with `until = now`, so each poll mints a key that will never be asked for
/// again — and this map is a process-lifetime static with no eviction anywhere
/// on the production path. Every minute the Quota lens stayed open therefore
/// left a whole window's messages resident for the life of the app.
///
/// **Per account**, not the whole map. That rationale is about one account's
/// own keys going stale each minute; it says nothing about another account's,
/// and a whole-map clear makes two configured accounts evict each other on
/// every poll. The result is still correct — a miss recomputes — so nothing
/// comparing totals can see it. It shows up only as a scan count.
///
/// Measured on the shipping build: ~39 MB per minute with the lens open, and
/// none of it returned when it closed — 507 MB before opening, 1451 MB after
/// ten minutes and a close. Clearing keeps the hit that matters (a second call
/// inside the same minute still finds this entry) and drops the ones that
/// cannot be hit again by construction.
///
/// Returns whether the entry was published, which is what the tests assert on.
pub(crate) fn publish(key: CacheKey, generation: u64, entry: CacheEntry) -> bool {
    let mut cache = crate::WINDOW_USAGE_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if crate::root_generation() != generation {
        return false;
    }
    cache.retain(|(account, _, _), _| account != &key.0);
    cache.insert(key, entry);
    true
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Message {
    timestamp: i64,
    client: String,
    provider_id: String,
    model_id: String,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
    cost: f64,
    is_turn_start: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WindowData {
    messages: Vec<Message>,
    undated_count: u32,
    processing_time_ms: u32,
}

pub(crate) fn run(
    context: &crate::LocalSourceContext,
    account: &Account,
    from_ms: i64,
    until_ms: i64,
) -> Result<Value, String> {
    let options = account_options(context, account)?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("build runtime: {}", e))?;
    SCAN_COUNT.fetch_add(1, Ordering::Relaxed);
    let usage = runtime.block_on(tokscale_core::get_window_usage(options, from_ms, until_ms))?;

    let data = WindowData {
        messages: usage
            .messages
            .into_iter()
            .map(|m| Message {
                timestamp: m.timestamp,
                client: m.client,
                provider_id: m.provider_id,
                model_id: m.model_id,
                input: m.input,
                output: m.output,
                cache_read: m.cache_read,
                cache_write: m.cache_write,
                reasoning: m.reasoning,
                cost: m.cost,
                is_turn_start: m.is_turn_start,
            })
            .collect(),
        undated_count: usage.undated_count,
        processing_time_ms: usage.processing_time_ms,
    };
    serde_json::to_value(data).map_err(|e| format!("serialize window usage: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::{LazyLock, Mutex};

    static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// The primary account, spelled out so a call site reads as a choice
    /// rather than as a `None` nobody thought about.
    const PRIMARY: Account = None;

    /// Hold every lock guarding state a per-account window scan reads.
    ///
    /// The registries are process-wide statics with their own module-local test
    /// locks, and `cargo test` runs modules concurrently: a test here that
    /// installed roots while holding only this module's lock raced
    /// `extra_scan_paths::tests`, and both sides saw the other's fixture. Taken
    /// in a fixed order — this module, then the scan registry, then the config
    /// dirs — because two tests acquiring the same pair in opposite orders is
    /// the other way to make a suite hang.
    fn lock_registries() -> (
        std::sync::MutexGuard<'static, ()>,
        std::sync::MutexGuard<'static, ()>,
        std::sync::MutexGuard<'static, ()>,
    ) {
        (
            TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
            crate::extra_scan_paths::TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            crate::claude_config_dirs::TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    #[test]
    fn quantised_window_calls_scan_once() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let from_ms = 1_700_000_000_000;
        let until_a = 1_700_000_060_001;
        let until_b = 1_700_000_060_999;
        let key = cache_key(&PRIMARY, from_ms, until_a);
        crate::WINDOW_USAGE_CACHE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&key);
        let before = scan_count();
        let context = crate::LocalSourceContext::for_home(PathBuf::from("/private/tmp"));

        cached(&context, &PRIMARY, from_ms, until_a).expect("first window scan");
        cached(&context, &PRIMARY, from_ms, until_b).expect("quantised cache hit");

        assert_eq!(
            cache_key(&PRIMARY, from_ms, until_a),
            cache_key(&PRIMARY, from_ms, until_b)
        );
        assert_eq!(scan_count(), before + 1);
        // If until_ms is no longer quantised, these two calls use different
        // keys and this assertion catches the accidental 0%-hit-rate change.
    }

    #[test]
    fn cache_keeps_only_the_newest_window() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let context = crate::LocalSourceContext::for_home(PathBuf::from("/private/tmp"));
        let from_ms = 1_700_000_000_000;
        crate::WINDOW_USAGE_CACHE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();

        // Three different minutes: exactly what a poll every 60s produces, and
        // what used to leave three whole scans resident for ever.
        for minute in 0..3 {
            cached(&context, &PRIMARY, from_ms, from_ms + 60_000 * (minute + 1))
                .expect("window scan");
        }
        let cache = crate::WINDOW_USAGE_CACHE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            cache.len(),
            1,
            "the map is a process-lifetime static with no eviction, so anything \
             this account keeps beyond its newest entry is kept until the app exits"
        );
    }

    /// One assistant turn with a distinct id, so a root's contribution is
    /// identifiable by its token count alone.
    fn turn(id: &str, output: i64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"2026-01-01T00:00:00.000Z","requestId":"req_{id}","message":{{"id":"msg_{id}","model":"claude-3-5-sonnet","usage":{{"input_tokens":0,"output_tokens":{output}}}}}}}"#
        )
    }

    fn write_session(root: &Path, id: &str, output: i64) {
        let dir = root.join("projects").join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{id}.jsonl")), turn(id, output)).unwrap();
    }

    fn output_tokens(payload: &Value) -> i64 {
        payload["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .map(|message| message["output"].as_i64().unwrap_or(0))
            .sum()
    }

    /// Primary at 1000, `work-d` at 7000 reachable by both the scan registry
    /// and a `.cc-mirror` variant, `work-e` at 500 reachable by the registry
    /// alone, `work-d/sub` at 90 reachable only by its own variant.
    ///
    /// Two extra accounts rather than one because a directory reachable two
    /// ways collapses into a single scan task and cannot tell the routes apart:
    /// `work-e` leaking is unambiguous evidence about the registry route.
    struct AccountFixture {
        _dir: tempfile::TempDir,
        home: PathBuf,
        d: PathBuf,
        e: PathBuf,
    }

    const PRIMARY_OUTPUT: i64 = 1_000;
    const D_OUTPUT: i64 = 7_000;
    const E_OUTPUT: i64 = 500;
    const SUB_OUTPUT: i64 = 90;

    fn account_fixture() -> AccountFixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let d = home.join("work-d");
        let e = home.join("work-e");
        let sub = d.join("sub");
        write_session(&home.join(".claude"), "primary", PRIMARY_OUTPUT);
        write_session(&d, "d", D_OUTPUT);
        write_session(&e, "e", E_OUTPUT);
        write_session(&sub, "sub", SUB_OUTPUT);
        for (name, config_dir) in [("v1", &d), ("v2", &sub)] {
            let variant = home.join(".cc-mirror").join(name);
            std::fs::create_dir_all(&variant).unwrap();
            std::fs::write(
                variant.join("variant.json"),
                serde_json::json!({ "configDir": config_dir.to_string_lossy() }).to_string(),
            )
            .unwrap();
        }
        // Both registries, installed through the real setters, because the
        // primary's exclusion is built from both and a test that populated one
        // would pass while the other went unread.
        let roots: Vec<String> = [&d, &e]
            .iter()
            .flat_map(|dir| {
                [
                    dir.join("projects").display().to_string(),
                    dir.join("transcripts").display().to_string(),
                ]
            })
            .collect();
        crate::extra_scan_paths::set_from_json(
            &serde_json::json!({ "claude": roots }).to_string(),
        )
        .unwrap();
        crate::claude_config_dirs::set_from_json(
            &serde_json::json!([d.display().to_string(), e.display().to_string()]).to_string(),
        )
        .unwrap();
        AccountFixture {
            _dir: dir,
            home,
            d,
            e,
        }
    }

    fn reset_registries() {
        crate::extra_scan_paths::reset_for_test();
        crate::claude_config_dirs::reset_for_test();
        crate::WINDOW_USAGE_CACHE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    const WINDOW_FROM: i64 = 1_767_225_600_000;
    const WINDOW_UNTIL: i64 = 1_767_312_000_000;

    fn scan(context: &crate::LocalSourceContext, account: &Account) -> Value {
        run(context, account, WINDOW_FROM, WINDOW_UNTIL).expect("window scan")
    }

    /// The defect issue #258 reports: with a second Claude account configured,
    /// the primary's window folds both accounts' transcripts.
    ///
    /// Asserted on both routes separately. `work-e` is reachable only through
    /// the scan registry and `work-d/sub` only through a `.cc-mirror` variant
    /// naming a directory below every registry entry, so a fix that closed one
    /// route and not the other fails here rather than passing on a machine
    /// that happens to have no mirrors.
    #[test]
    fn primary_window_excludes_every_configured_extra_account() {
        let _guards = lock_registries();
        reset_registries();
        let fixture = account_fixture();
        let context = crate::LocalSourceContext::for_home(fixture.home.clone());

        let primary = scan(&context, &PRIMARY);

        assert_eq!(
            output_tokens(&primary),
            PRIMARY_OUTPUT,
            "the primary window folded another account's transcripts: {primary}"
        );
        reset_registries();
    }

    /// The control for the test above: without the exclusion the fixture really
    /// does deliver all four roots, so a green result there is the exclusion
    /// working rather than the fixture being inert.
    #[test]
    fn the_fixture_delivers_every_root_before_the_exclusion_is_applied() {
        let _guards = lock_registries();
        reset_registries();
        let fixture = account_fixture();
        let context = crate::LocalSourceContext::for_home(fixture.home.clone());

        let wide = run(
            &context,
            &PRIMARY,
            WINDOW_FROM,
            WINDOW_UNTIL,
        );
        let _ = wide;
        // Read the roots the way the shipping primary selector does NOT: the
        // unnarrowed options every caller used before this change.
        let options = context.report_options(None, None);
        let usage = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(tokscale_core::get_window_usage(
                options,
                WINDOW_FROM,
                WINDOW_UNTIL,
            ))
            .expect("wide scan");
        let total: i64 = usage.messages.iter().map(|message| message.output).sum();

        assert_eq!(
            total,
            PRIMARY_OUTPUT + D_OUTPUT + E_OUTPUT + SUB_OUTPUT,
            "the fixture does not reach every root, so the exclusion test above \
             could pass without excluding anything"
        );
        reset_registries();
    }

    /// The two registries are installed by separate FFI calls, and
    /// `ClaudeExtraRoots.install` discards the first call's result, so they can
    /// disagree: a directory can reach `tb_set_extra_scan_paths` while
    /// `tb_set_claude_config_dirs` still holds the previous list.
    ///
    /// The primary must still refuse it. Without the registry half of
    /// `primary_exclusions` this is the shape that leaks — and it is the only
    /// shape that does, which is why it needs its own fixture rather than
    /// riding on the one above where both registries agree.
    #[test]
    fn primary_window_excludes_a_root_the_config_dir_registry_never_heard_of() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let orphan = home.join("work-orphan");
        write_session(&home.join(".claude"), "primary", PRIMARY_OUTPUT);
        write_session(&orphan, "orphan", E_OUTPUT);

        // Scan registry knows the account; identity registry does not.
        crate::extra_scan_paths::set_from_json(
            &serde_json::json!({
                "claude": [orphan.join("projects").display().to_string()]
            })
            .to_string(),
        )
        .unwrap();
        assert!(
            crate::claude_config_dirs::snapshot().is_empty(),
            "the fixture is meant to leave the identity registry empty"
        );

        let context = crate::LocalSourceContext::for_home(home);
        let primary = scan(&context, &PRIMARY);

        assert_eq!(
            output_tokens(&primary),
            PRIMARY_OUTPUT,
            "a scan root absent from the config-dir registry was folded into \
             the primary's window: {primary}"
        );
        reset_registries();
    }

    /// Each extra account's window is its own transcripts and nothing else —
    /// not the primary's, and not the other extra account's.
    #[test]
    fn an_extra_account_window_is_exactly_that_account() {
        let _guards = lock_registries();
        reset_registries();
        let fixture = account_fixture();
        let context = crate::LocalSourceContext::for_home(fixture.home.clone());

        let d = scan(&context, &Some(fixture.d.display().to_string()));
        let e = scan(&context, &Some(fixture.e.display().to_string()));

        assert_eq!(
            output_tokens(&d),
            D_OUTPUT,
            "account D's window is not its own registered roots: {d}"
        );
        // `work-d/sub` belongs to neither window, and that is the intended
        // answer rather than a leak in the other direction. It is a config
        // directory of its own, discovered through a `.cc-mirror` variant and
        // never configured in Syrtis, so it is not the primary's usage and
        // not D's — it is a third account nobody registered. Today it silently
        // inflates the primary's window; counting it for no quota is the honest
        // result, because there is no quota reading to divide it against.
        assert!(
            output_tokens(&d) != D_OUTPUT + SUB_OUTPUT,
            "an unregistered nested account was folded into D's window: {d}"
        );
        assert_eq!(
            output_tokens(&e),
            E_OUTPUT,
            "account E's window is not its own roots: {e}"
        );
        reset_registries();
    }

    /// Restores `TOKSCALE_EXTRA_DIRS` on drop, so a panicking assertion cannot
    /// leave the variable set for whatever test runs next.
    struct ExtraDirsGuard(Option<std::ffi::OsString>);

    impl ExtraDirsGuard {
        fn set(value: &str) -> Self {
            let guard = Self(std::env::var_os("TOKSCALE_EXTRA_DIRS"));
            unsafe { std::env::set_var("TOKSCALE_EXTRA_DIRS", value) };
            guard
        }
    }

    impl Drop for ExtraDirsGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(previous) => unsafe {
                    std::env::set_var("TOKSCALE_EXTRA_DIRS", previous)
                },
                None => unsafe { std::env::remove_var("TOKSCALE_EXTRA_DIRS") },
            }
        }
    }

    /// `TOKSCALE_EXTRA_DIRS` names `claude:` roots of its own, and an extra
    /// account's window must not absorb them.
    ///
    /// This is the only property `use_env_roots: false` buys on this path, and
    /// it is worth pinning precisely because the flag reads as if it bought
    /// more: Claude's declared root is `PathRoot::Home`, which the flag does
    /// not gate at all.
    #[test]
    fn an_extra_account_window_ignores_env_named_roots() {
        let _guards = lock_registries();
        reset_registries();
        let fixture = account_fixture();
        let stray = fixture.home.join("stray");
        write_session(&stray, "stray", SUB_OUTPUT);
        let _env = ExtraDirsGuard::set(&format!(
            "claude:{}",
            stray.join("projects").display()
        ));

        let context = crate::LocalSourceContext::for_home(fixture.home.clone());
        let d = scan(&context, &Some(fixture.d.display().to_string()));

        assert_eq!(
            output_tokens(&d),
            D_OUTPUT,
            "an environment-named root was folded into account D's window: {d}"
        );
        reset_registries();
    }

    /// Two CONFIGURED accounts where one directory sits inside the other.
    ///
    /// Settings permits it — `isRejectedRoot` refuses only the empty path, `/`
    /// and the home directory — so `/work` and `/work/sub` can both be real
    /// accounts with real credentials. Selecting an account's roots by prefix
    /// would give the outer one the inner one's transcripts, which is this
    /// issue's defect reappearing on the selection side after being fixed on
    /// the exclusion side.
    ///
    /// Distinct from the nested case in `an_extra_account_window_is_exactly_that_account`:
    /// there the inner directory is registered with neither registry and
    /// belongs to no window, and it takes the other branch entirely.
    #[test]
    fn a_nested_configured_account_is_not_folded_into_the_outer_one() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let outer = home.join("work");
        let inner = outer.join("sub");
        write_session(&home.join(".claude"), "primary", PRIMARY_OUTPUT);
        write_session(&outer, "outer", D_OUTPUT);
        write_session(&inner, "inner", E_OUTPUT);

        let roots: Vec<String> = [&outer, &inner]
            .iter()
            .flat_map(|d| {
                [
                    d.join("projects").display().to_string(),
                    d.join("transcripts").display().to_string(),
                ]
            })
            .collect();
        crate::extra_scan_paths::set_from_json(
            &serde_json::json!({ "claude": roots }).to_string(),
        )
        .unwrap();
        crate::claude_config_dirs::set_from_json(
            &serde_json::json!([
                outer.display().to_string(),
                inner.display().to_string()
            ])
            .to_string(),
        )
        .unwrap();

        let context = crate::LocalSourceContext::for_home(home.clone());
        let outer_window = scan(&context, &Some(outer.display().to_string()));
        let inner_window = scan(&context, &Some(inner.display().to_string()));
        let primary = scan(&context, &PRIMARY);

        assert_eq!(
            output_tokens(&outer_window),
            D_OUTPUT,
            "the outer account folded the nested account's transcripts: {outer_window}"
        );
        assert_eq!(
            output_tokens(&inner_window),
            E_OUTPUT,
            "the nested account's window is not its own roots: {inner_window}"
        );
        assert_eq!(
            output_tokens(&primary),
            PRIMARY_OUTPUT,
            "the primary folded a configured account: {primary}"
        );
        reset_registries();
    }

    const INNER_OUTPUT: i64 = 3_000;

    fn claude_roots(dirs: &[&PathBuf]) -> Vec<String> {
        dirs.iter()
            .flat_map(|d| {
                [
                    d.join("projects").display().to_string(),
                    d.join("transcripts").display().to_string(),
                ]
            })
            .collect()
    }

    fn install(roots: &[String], config_dirs: &[&PathBuf]) {
        crate::extra_scan_paths::set_from_json(
            &serde_json::json!({ "claude": roots }).to_string(),
        )
        .unwrap();
        crate::claude_config_dirs::set_from_json(
            &serde_json::json!(config_dirs
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>())
            .to_string(),
        )
        .unwrap();
    }

    /// An account configured at `D/.claude` beside `D` itself. `D`'s window is
    /// scanned with `D` as home, and the engine resolves `D/.claude/projects`
    /// on its own, so before the nested-account exclusion `D` read 10,000:
    /// its own 7,000 and the inner account's 3,000, which the inner window
    /// also counted.
    #[test]
    fn an_account_nested_at_dot_claude_is_not_counted_in_the_outer_window() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let d = home.join("work-d");
        let inner = d.join(".claude");
        write_session(&home.join(".claude"), "primary", PRIMARY_OUTPUT);
        write_session(&d, "d", D_OUTPUT);
        write_session(&inner, "inner", INNER_OUTPUT);
        install(&claude_roots(&[&d, &inner]), &[&d, &inner]);

        let context = crate::LocalSourceContext::for_home(home.clone());
        let outer = scan(&context, &Some(d.display().to_string()));
        let nested = scan(&context, &Some(inner.display().to_string()));
        let primary = scan(&context, &PRIMARY);

        assert_eq!(
            output_tokens(&outer),
            D_OUTPUT,
            "the outer account counted the account nested at .claude: {outer}"
        );
        assert_eq!(output_tokens(&nested), INNER_OUTPUT, "{nested}");
        assert_eq!(output_tokens(&primary), PRIMARY_OUTPUT, "{primary}");
        reset_registries();
    }

    /// The other half of the nested exclusion: an account at `D/.claude`
    /// whose roots reached the scan registry while its directory did not reach
    /// the config-dir registry. `ClaudeExtraRoots.install` sets the two
    /// separately and the lists can disagree (`primary_exclusions`), so the
    /// config-dir half alone would leave this one counted in D.
    #[test]
    fn a_nested_account_known_only_by_its_roots_is_excluded_too() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let d = home.join("work-d");
        let inner = d.join(".claude");
        write_session(&d, "d", D_OUTPUT);
        write_session(&inner, "inner", INNER_OUTPUT);
        install(&claude_roots(&[&d, &inner]), &[&d]);

        let context = crate::LocalSourceContext::for_home(home.clone());
        let outer = scan(&context, &Some(d.display().to_string()));

        assert_eq!(output_tokens(&outer), D_OUTPUT, "{outer}");
        reset_registries();
    }

    /// A configured `D/.claude` that is a symlink back to `D` is nested only as
    /// spelled. The engine canonicalises exclusions, so listing it would cover
    /// D's own roots and read D as zero; it is left out instead.
    #[cfg(unix)]
    #[test]
    fn a_nested_path_that_resolves_to_the_account_itself_is_not_excluded() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let d = home.join("work-d");
        write_session(&d, "d", D_OUTPUT);
        let alias = d.join(".claude");
        std::os::unix::fs::symlink(&d, &alias).unwrap();
        install(&claude_roots(&[&d, &alias]), &[&d, &alias]);

        let context = crate::LocalSourceContext::for_home(home.clone());
        let outer = scan(&context, &Some(d.display().to_string()));

        assert!(output_tokens(&outer) >= D_OUTPUT, "D's own roots were excluded: {outer}");
        reset_registries();
    }

    /// A `.cc-mirror` variant under an extra account's directory may name a
    /// directory anywhere; its rows carry `cc-mirror/<variant>`, and a usage
    /// attribution declaring that id against Claude would put them on this
    /// account's card. The account window asks for exactly `claude`.
    #[test]
    fn an_extra_account_window_reads_only_the_claude_client() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let d = home.join("work-d");
        let elsewhere = home.join("elsewhere");
        write_session(&d, "d", D_OUTPUT);
        write_session(&elsewhere, "mirror", SUB_OUTPUT);
        let variant = d.join(".cc-mirror").join("v");
        std::fs::create_dir_all(&variant).unwrap();
        std::fs::write(
            variant.join("variant.json"),
            serde_json::json!({ "configDir": elsewhere.to_string_lossy() }).to_string(),
        )
        .unwrap();
        install(&claude_roots(&[&d]), &[&d]);

        let context = crate::LocalSourceContext::for_home(home.clone());
        let window = scan(&context, &Some(d.display().to_string()));

        assert_eq!(output_tokens(&window), D_OUTPUT, "{window}");
        assert!(
            window["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m["client"] == CLAUDE),
            "a non-Claude client reached the account window: {window}"
        );
        reset_registries();
    }

    /// With nothing nested and no other client under the account, the window
    /// is exactly what the options before the nested-account exclusion
    /// produced: same messages, same undated count.
    #[test]
    fn an_unnested_account_window_is_unchanged_by_the_exclusion() {
        let _guards = lock_registries();
        reset_registries();
        let fixture = account_fixture();
        let context = crate::LocalSourceContext::for_home(fixture.home.clone());
        for account in [&fixture.d, &fixture.e] {
            let dir = account.display().to_string();
            let before = tokscale_core::ReportOptions {
                home_dir: Some(dir.clone()),
                use_env_roots: false,
                scanner_settings: tokscale_core::scanner::ScannerSettings {
                    extra_scan_paths: BTreeMap::from([(
                        CLAUDE.to_string(),
                        registered_roots_under(&dir),
                    )]),
                    ..Default::default()
                },
                ..Default::default()
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let old = runtime
                .block_on(tokscale_core::get_window_usage(before, WINDOW_FROM, WINDOW_UNTIL))
                .unwrap();
            let now = scan(&context, &Some(dir.clone()));
            let old_messages: Vec<(i64, String, i64)> = old
                .messages
                .iter()
                .map(|m| (m.timestamp, m.client.clone(), m.output))
                .collect();
            let new_messages: Vec<(i64, String, i64)> = now["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| {
                    (
                        m["timestamp"].as_i64().unwrap(),
                        m["client"].as_str().unwrap().to_string(),
                        m["output"].as_i64().unwrap(),
                    )
                })
                .collect();
            assert!(!old_messages.is_empty(), "{dir}: the control read nothing");
            assert_eq!(old_messages, new_messages, "{dir}");
            assert_eq!(
                i64::from(old.undated_count),
                now["undatedCount"].as_i64().unwrap(),
                "{dir}"
            );
        }
        reset_registries();
    }

    /// A configured account whose roots are not registered yet gets a failure,
    /// not an empty window.
    ///
    /// `ClaudeExtraRoots.install` installs the two registries in separate FFI
    /// calls and wakes the quota pollers between them, so a just-added account
    /// is briefly in `claude_config_dirs` and absent from `extra_scan_paths`;
    /// an account whose roots the scan setter refused outright stays that way.
    /// Rendering the empty scan would state that the account used nothing,
    /// which is the same class of wrong answer this file exists to remove.
    #[test]
    fn an_account_with_no_registered_roots_fails_rather_than_reporting_zero() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let pending = home.join("work-pending");
        write_session(&home.join(".claude"), "primary", PRIMARY_OUTPUT);
        write_session(&pending, "pending", D_OUTPUT);

        // The account is configured; its scan roots have not landed.
        crate::claude_config_dirs::set_from_json(
            &serde_json::json!([pending.display().to_string()]).to_string(),
        )
        .unwrap();
        assert!(
            crate::extra_scan_paths::snapshot().is_empty(),
            "the fixture is meant to leave the scan registry empty"
        );

        let context = crate::LocalSourceContext::for_home(home.clone());
        let account = Some(pending.display().to_string());
        let refused = run(&context, &account, WINDOW_FROM, WINDOW_UNTIL);

        assert!(
            refused.is_err(),
            "an unscannable account reported a window instead of failing: {refused:?}"
        );

        // The control: once the roots are registered the same account answers,
        // so the refusal above is about the missing registry entry and not
        // about the account being unreadable in general.
        crate::extra_scan_paths::set_from_json(
            &serde_json::json!({
                "claude": [pending.join("projects").display().to_string()]
            })
            .to_string(),
        )
        .unwrap();
        let answered = run(&context, &account, WINDOW_FROM, WINDOW_UNTIL).expect("registered");
        assert_eq!(output_tokens(&answered), D_OUTPUT);
        reset_registries();
    }

    /// One account's cached window must never answer a request for another's.
    #[test]
    fn a_cached_window_does_not_answer_for_another_account() {
        let _guards = lock_registries();
        reset_registries();
        let fixture = account_fixture();
        let context = crate::LocalSourceContext::for_home(fixture.home.clone());
        let d = Some(fixture.d.display().to_string());

        let first = cached(&context, &PRIMARY, WINDOW_FROM, WINDOW_UNTIL).expect("primary");
        let second = cached(&context, &d, WINDOW_FROM, WINDOW_UNTIL).expect("account D");

        assert_eq!(output_tokens(&first), PRIMARY_OUTPUT);
        assert_eq!(
            output_tokens(&second),
            D_OUTPUT,
            "account D was served the primary's cached window: {second}"
        );
        reset_registries();
    }

    /// Two configured accounts polled in the same minute must cost two scans,
    /// not one per call.
    ///
    /// A counter rather than a comparison, deliberately. Clearing the whole map
    /// on every publish evicts but never lies — the re-scan returns correct
    /// data — so no assertion on tokens can see it, and the cost lands on a
    /// scan measured in seconds on a real store.
    #[test]
    fn two_accounts_polled_in_one_minute_cost_two_scans() {
        let _guards = lock_registries();
        reset_registries();
        let fixture = account_fixture();
        let context = crate::LocalSourceContext::for_home(fixture.home.clone());
        let d = Some(fixture.d.display().to_string());

        let before = scan_count();
        cached(&context, &PRIMARY, WINDOW_FROM, WINDOW_UNTIL).expect("primary");
        cached(&context, &d, WINDOW_FROM, WINDOW_UNTIL).expect("account D");
        cached(&context, &PRIMARY, WINDOW_FROM, WINDOW_UNTIL).expect("primary again");

        assert_eq!(
            scan_count() - before,
            2,
            "the second account's publish evicted the first account's entry, so \
             re-asking for it paid a whole scan again"
        );
        reset_registries();
    }

    #[test]
    fn different_minute_uses_different_key() {
        assert_ne!(
            cache_key(&PRIMARY, 1_700_000_000_000, 1_700_000_060_001),
            cache_key(&PRIMARY, 1_700_000_000_000, 1_700_000_120_001)
        );
    }
}
