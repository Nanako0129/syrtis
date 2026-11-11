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
/// **Direct children only, not every descendant.** The config-dir rule
/// (`claude_config_dirs::check`) refuses `/work/sub` beside `/work`, but the
/// scan registry is a separate setter with no such rule, and an older build's
/// stored list can still hold both — and a prefix test would then hand `/work`
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
///
/// Never excludes a path that collides with the primary's own roots
/// (`claude_config_dirs::PrimaryRoots::collides` under `home`), whatever a
/// registry holds. The config-dir setter and Swift
/// refuse those directories, but the scan registry has no such rule and either
/// registry alone was enough to empty the primary window (audit P1, 2026-11-03),
/// so this is the guard that does not depend on both callers getting it right.
///
/// `collides` compares spelling only. The engine canonicalises an exclusion and
/// matches each file both as found and canonicalised, so an entry that is
/// another folder as spelled but resolves into a primary root (an extra
/// account whose `projects` is a symlink to `~/.claude/projects`, or the whole
/// account folder a symlink to `~/.claude`) would still empty the primary
/// window. Such an entry is dropped too, judged by canonical path: one that
/// resolves to a scan root's own parent folder (`~/.claude` for its roots,
/// Claude Desktop's folder, a cc-mirror variant's config), or to a scan root
/// or under one, so the primary keeps reading those roots. That covers an
/// alias made on either side: an account folder linked to `~/.claude`, and
/// the real target of a `~/.claude` that is itself a link (a dotfiles
/// layout). For an alias of `~/.claude` or of `~/.claude/projects` the extra
/// card reads the same shared transcripts (measured in the tests); what an
/// alias of Desktop's folder or a variant's config reads on its own card was
/// not exercised.
///
/// The root test applies only to a root that resolves inside its own parent.
/// A root that is itself a link out of it (`~/.claude/projects` ->
/// `work/projects/shared` or `work`) lives in a real extra account folder;
/// dropping that account's entries would let the primary scan, which also
/// walks the registered extra roots, count the account's other transcripts.
/// They are kept, and the linked root reads 0 on the primary card, as a linked
/// file does. An entry that only contains a resolved parent is kept for the
/// same reason, so when `~/.claude` itself links into an account folder
/// (`~/.claude` -> `work/shared`) every primary root resolves under the kept
/// `work` and the whole primary card reads 0
/// (`a_primary_transcript_symlinked_into_an_extra_account_is_not_the_primarys`,
/// `a_primary_root_linked_into_an_extra_account_keeps_the_account_out`,
/// `a_primary_side_linked_into_an_account_keeps_both_registries_out`).
fn primary_exclusions(home: Option<&Path>) -> Vec<PathBuf> {
    let mut excluded: Vec<PathBuf> = crate::claude_config_dirs::snapshot()
        .into_iter()
        .map(PathBuf::from)
        .collect();
    if let Some(roots) = crate::extra_scan_paths::snapshot().get(CLAUDE) {
        excluded.extend(roots.iter().cloned());
    }
    if let (Some(home), false) = (home, excluded.is_empty()) {
        // Read once for the whole list, not once per path.
        let primary = crate::claude_config_dirs::PrimaryRoots::for_home(home);
        excluded.retain(|path| !primary.collides(path));
        // Each scan root's own parent, resolved, and the root resolved when it
        // stays inside that parent. A folder inside `~/.claude` but outside the
        // scan roots, such as `~/.claude/accounts/work`, is neither and stays
        // an account like any other. A root with no parent (none exists today)
        // is skipped.
        let roots: Vec<(Option<PathBuf>, PathBuf)> = primary
            .roots()
            .iter()
            .filter_map(|root| {
                let parent = canonical(root.parent()?);
                let resolved = canonical(root);
                Some((resolved.starts_with(&parent).then_some(resolved), parent))
            })
            .collect();
        excluded.retain(|path| {
            let resolved = canonical(path);
            !roots.iter().any(|(root, parent)| {
                resolved == *parent || root.as_ref().is_some_and(|root| resolved.starts_with(root))
            })
        });
    }
    excluded
}

/// `path` with symlinks resolved, or as given when it cannot be resolved (a
/// root that does not exist yet). Runs off the main thread, like every caller.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
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
    // The engine matches a file against an exclusion both as found (lexically
    // normalised) and canonicalised, and canonicalises the exclusion too, so
    // one that is nested only as spelled (`D/.claude` a symlink back to `D` or
    // above) would cover this account's own roots and empty its window. Such an
    // entry is dropped: counted twice beats confidently zero.
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
            let excluded = primary_exclusions(context.home_dir.as_deref());
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
    const COWORK_OUTPUT: i64 = 300;
    const INSIDE_CLAUDE_OUTPUT: i64 = 70;
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
    /// The config-dir rule now refuses the nested one (`overlap`), but the scan
    /// registry has no such rule and an older build's stored list can hold
    /// both, so `/work` and `/work/sub` can still both have registered roots.
    /// Selecting an account's roots by prefix
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
    /// spelled. The engine also matches an exclusion by its canonical form, so
    /// listing it would cover D's own roots and read D as zero; it is left out
    /// instead.
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

        assert_eq!(output_tokens(&outer), D_OUTPUT, "D's own roots were excluded: {outer}");
        reset_registries();
    }

    /// A primary transcript that is a symlink into an extra account's folder.
    /// The engine judges each file both as found and by its canonical path, so
    /// the link is the extra account's file and the primary's window leaves it
    /// out. At the previous pin `15204f4`, before engine PR #76 (merged as
    /// `76ee5d4`), only the scan root was compared, so the primary counted the
    /// link's target a second time. The extra account's own window still reads
    /// its target.
    ///
    /// The control at the end is what makes the first assertion mean something:
    /// with no extra account registered and no `.cc-mirror` variant, nothing
    /// excludes the link, and the primary reads it (1,000 + 7,000).
    #[cfg(unix)]
    #[test]
    fn a_primary_transcript_symlinked_into_an_extra_account_is_not_the_primarys() {
        let _guards = lock_registries();
        reset_registries();
        let fixture = account_fixture();
        let target = fixture.d.join("projects/proj/d.jsonl");
        assert!(target.is_file(), "the fixture has no target to link to");
        let link = fixture.home.join(".claude/projects/proj/link.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        // Fixture sources only: a CLAUDE_CONFIG_DIR or TOKSCALE_EXTRA_DIRS in the
        // test process's environment must not add roots to either scan.
        let context = crate::LocalSourceContext {
            home_dir: Some(fixture.home.clone()),
            use_env_roots: false,
        };

        let primary = scan(&context, &PRIMARY);
        let extra = scan(&context, &Some(fixture.d.display().to_string()));

        assert_eq!(
            output_tokens(&primary),
            PRIMARY_OUTPUT,
            "the primary counted a transcript that lives in an extra account: {primary}"
        );
        assert_eq!(output_tokens(&extra), D_OUTPUT, "{extra}");

        // Control: drop the exclusion's inputs (both registries, and the
        // variants that would reach `work-d` by another route, which would
        // read the same message and hide the link behind a dedup). The link is
        // then walked and counted, so the 1,000 above is the exclusion at work.
        reset_registries();
        std::fs::remove_dir_all(fixture.home.join(".cc-mirror")).unwrap();
        let unexcluded = scan(&context, &PRIMARY);
        assert_eq!(
            output_tokens(&unexcluded),
            PRIMARY_OUTPUT + D_OUTPUT,
            "without the exclusion the symlinked transcript is not walked, so \
             the assertion above proves nothing: {unexcluded}"
        );
        reset_registries();
    }

    /// Extra accounts that share the primary's transcripts through a symlink:
    /// a real account folder whose `projects` links to `~/.claude/projects`
    /// (the maintainer's setup), an account folder that is itself a link to
    /// `~/.claude`, and a `projects` linked to one project folder inside
    /// `~/.claude/projects`. No spelling collides with a primary root, but the
    /// engine resolves the exclusion, so before the canonical guard in
    /// `primary_exclusions` the primary window read 0 in each. With it, the
    /// primary reads its own transcript; the extra card of the first two reads
    /// the same shared transcript, since nothing in a transcript names its
    /// account. Mutations: delete the canonical `retain` in
    /// `primary_exclusions` (every case reads 0); drop the parent test
    /// (`whole_folder` reads 0).
    #[cfg(unix)]
    #[test]
    fn an_extra_account_linked_to_the_primarys_transcripts_does_not_empty_the_primary() {
        for shape in ["projects_link", "whole_folder", "projects_into_a_project"] {
            let _guards = lock_registries();
            reset_registries();
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().to_path_buf();
            write_session(&home.join(".claude"), "primary", PRIMARY_OUTPUT);
            let extra = home.join("claude-team");
            let link_target = match shape {
                "whole_folder" => None,
                "projects_link" => Some(home.join(".claude/projects")),
                _ => Some(home.join(".claude/projects/proj")),
            };
            match &link_target {
                None => std::os::unix::fs::symlink(home.join(".claude"), &extra).unwrap(),
                Some(target) => {
                    std::fs::create_dir_all(&extra).unwrap();
                    std::os::unix::fs::symlink(target, extra.join("projects")).unwrap();
                }
            }
            crate::extra_scan_paths::set_from_json(
                &serde_json::json!({ "claude": [
                    extra.join("projects").display().to_string(),
                    extra.join("transcripts").display().to_string(),
                ] })
                .to_string(),
            )
            .unwrap();
            crate::claude_config_dirs::set_from_json(
                &serde_json::json!([extra.display().to_string()]).to_string(),
            )
            .unwrap();
            let context = crate::LocalSourceContext {
                home_dir: Some(home.clone()),
                use_env_roots: false,
            };

            let primary = scan(&context, &PRIMARY);
            let shared = scan(&context, &Some(extra.display().to_string()));

            assert_eq!(
                output_tokens(&primary),
                PRIMARY_OUTPUT,
                "{shape}: the primary window lost its own transcript: {primary}"
            );
            if shape != "projects_into_a_project" {
                assert_eq!(output_tokens(&shared), PRIMARY_OUTPUT, "{shape}: {shared}");
            }
            reset_registries();
        }
    }

    /// An extra account folder that is a symlink to the parent of a primary
    /// scan root outside `~/.claude` (Claude Desktop's folder) is not excluded
    /// from the primary window, while a genuine account next to it still is.
    /// This checks the exclusion list only; no Cowork transcript is scanned.
    /// Mutation: drop the parent test in `primary_exclusions` (the alias is
    /// listed).
    #[cfg(unix)]
    #[test]
    fn an_alias_of_a_primary_roots_parent_is_not_excluded() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let desktop = home.join("Library/Application Support/Claude");
        std::fs::create_dir_all(desktop.join("local-agent-mode-sessions")).unwrap();
        std::fs::create_dir_all(home.join(".claude/projects")).unwrap();
        let alias = home.join("claude-desktop");
        std::os::unix::fs::symlink(&desktop, &alias).unwrap();
        let genuine = home.join("work");
        std::fs::create_dir_all(genuine.join("projects")).unwrap();
        crate::claude_config_dirs::set_from_json(
            &serde_json::json!([alias.display().to_string(), genuine.display().to_string()])
                .to_string(),
        )
        .unwrap();

        let excluded = primary_exclusions(Some(&home));

        assert!(!excluded.contains(&alias), "{excluded:?}");
        assert!(excluded.contains(&genuine), "{excluded:?}");
        reset_registries();
    }

    /// `~/.claude` or one of its roots links into another folder `work`, which
    /// is registered in both registries. (a) `~/.claude/projects` -> `work`:
    /// the root lives in a real account, so `work` stays excluded; the primary
    /// card keeps only its unlinked `~/.claude/transcripts` (500). (b)
    /// `~/.claude` -> `work/shared`: every primary root resolves under the
    /// kept `work`, so the whole primary card reads 0. (c) `~/.claude` ->
    /// `work` (a dotfiles layout; `work` is the primary's own folder): `work`
    /// is the resolved `~/.claude` and is dropped, so the primary keeps its
    /// 1,000 and the account card reads the same shared 1,000. In (a) and (b)
    /// the account card reads `work/transcripts` (7,000). Mutations (measured,
    /// each failing a shape): no parent equality (c), no root test (c), the
    /// root test without its inside-the-parent check (a: 8,500), a parent that
    /// is merely contained (b: 1,000).
    #[cfg(unix)]
    #[test]
    fn a_primary_side_linked_into_an_account_keeps_both_registries_out() {
        for (shape, primary_output, account_output) in [
            ("projects_to_account", 500, D_OUTPUT),
            ("claude_to_account_subfolder", 0, D_OUTPUT),
            (
                "claude_to_its_dotfiles_target",
                PRIMARY_OUTPUT,
                PRIMARY_OUTPUT,
            ),
        ] {
            let _guards = lock_registries();
            reset_registries();
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().to_path_buf();
            let work = home.join("work");
            let seed = |folder: PathBuf, id: &str, output: i64| {
                std::fs::create_dir_all(&folder).unwrap();
                std::fs::write(folder.join(format!("{id}.jsonl")), turn(id, output)).unwrap();
            };
            match shape {
                "projects_to_account" => {
                    seed(work.join("transcripts/t"), "w", D_OUTPUT);
                    seed(work.join("p"), "a", PRIMARY_OUTPUT);
                    seed(home.join(".claude/transcripts/t"), "c", 500);
                    std::os::unix::fs::symlink(&work, home.join(".claude/projects")).unwrap();
                }
                "claude_to_account_subfolder" => {
                    seed(work.join("transcripts/t"), "w", D_OUTPUT);
                    seed(work.join("shared/projects/p"), "a", PRIMARY_OUTPUT);
                    std::os::unix::fs::symlink(work.join("shared"), home.join(".claude")).unwrap();
                }
                _ => {
                    seed(work.join("projects/p"), "a", PRIMARY_OUTPUT);
                    std::os::unix::fs::symlink(&work, home.join(".claude")).unwrap();
                }
            }
            crate::extra_scan_paths::set_from_json(
                &serde_json::json!({ "claude": [
                    work.join("projects").display().to_string(),
                    work.join("transcripts").display().to_string(),
                ] })
                .to_string(),
            )
            .unwrap();
            crate::claude_config_dirs::set_from_json(
                &serde_json::json!([work.display().to_string()]).to_string(),
            )
            .unwrap();
            let context = crate::LocalSourceContext {
                home_dir: Some(home.clone()),
                use_env_roots: false,
            };

            let primary = scan(&context, &PRIMARY);
            let account = scan(&context, &Some(work.display().to_string()));

            assert_eq!(
                output_tokens(&primary),
                primary_output,
                "{shape}: primary {primary}"
            );
            assert_eq!(
                output_tokens(&account),
                account_output,
                "{shape}: account {account}"
            );
            reset_registries();
        }
    }

    /// The reverse layout: the primary's `~/.claude/projects` is itself a link
    /// to one project folder (`shared`) inside a real extra account `work`
    /// that also holds another project (`other`). The exclusions for `work`
    /// and its roots only contain the primary root, so they are kept: the
    /// primary card counts neither `other` (reachable through the registered
    /// extra root `work/projects`) nor the linked `shared` (that account's, as
    /// #537 decided for a linked file). Mutation (measured): also drop an
    /// exclusion that contains a resolved primary root, without the check that
    /// the root resolves inside its parent; the primary then reads 8,000
    /// (`shared` + `other`).
    #[cfg(unix)]
    #[test]
    fn a_primary_root_linked_into_an_extra_account_keeps_the_account_out() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let work = home.join("work");
        for (project, id, output) in [("shared", "s", PRIMARY_OUTPUT), ("other", "o", D_OUTPUT)] {
            let folder = work.join("projects").join(project);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join(format!("{id}.jsonl")), turn(id, output)).unwrap();
        }
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::os::unix::fs::symlink(work.join("projects/shared"), home.join(".claude/projects"))
            .unwrap();
        crate::extra_scan_paths::set_from_json(
            &serde_json::json!({ "claude": [
                work.join("projects").display().to_string(),
                work.join("transcripts").display().to_string(),
            ] })
            .to_string(),
        )
        .unwrap();
        crate::claude_config_dirs::set_from_json(
            &serde_json::json!([work.display().to_string()]).to_string(),
        )
        .unwrap();
        let context = crate::LocalSourceContext {
            home_dir: Some(home.clone()),
            use_env_roots: false,
        };

        let primary = scan(&context, &PRIMARY);
        let account = scan(&context, &Some(work.display().to_string()));

        assert_eq!(
            output_tokens(&primary),
            0,
            "the primary counted the extra account's projects: {primary}"
        );
        assert_eq!(
            output_tokens(&account),
            PRIMARY_OUTPUT + D_OUTPUT,
            "{account}"
        );
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

    /// An extra account's options must carry that account's directory, client
    /// filter and scanner settings; dropping any field re-scopes the window.
    #[test]
    fn an_extra_account_report_is_rooted_at_that_account() {
        let _guards = lock_registries();
        reset_registries();
        let fixture = account_fixture();
        let context = crate::LocalSourceContext::for_home(fixture.home.clone());
        let account = Some(fixture.d.display().to_string());

        let report = account_options(&context, &account).expect("registered account");
        assert_eq!(
            report.home_dir.as_deref(),
            Some(fixture.d.to_string_lossy().as_ref()),
            "the extra account's report is not rooted at its own directory"
        );
        assert_eq!(report.clients, Some(vec![CLAUDE.to_string()]));

        let parse = account_parse_options(&context, &account).expect("registered account");
        assert_eq!(parse.home_dir, report.home_dir);
        assert_eq!(parse.clients, report.clients);
        assert_eq!(
            parse.scanner_settings.extra_scan_paths,
            report.scanner_settings.extra_scan_paths
        );
        reset_registries();
    }

    /// Audit P1 (2026-11-03), reproduced on c6be6549 before the fix: the user's
    /// own `<home>/.claude` registered as an extra Claude account emptied the
    /// primary window. The config-dir setter accepted it, and
    /// `ClaudeExtraRoots.install` sent `<dir>/projects` and `<dir>/transcripts`
    /// to the scan registry whatever the first setter answered; either registry
    /// alone made `primary_exclusions` remove the primary's own transcripts.
    /// Three cases for `~/.claude`: (a) config dirs only, (b) scan registry only,
    /// (c) both, as production installs. Then, both registries, every other
    /// shape of primary root (S6): `<home>` (ancestor), `~/.claude/projects`
    /// (a root itself), `~/.claude/projects/x` (inside one),
    /// `~/Library/Application Support/Claude` (ancestor of the Cowork tree,
    /// whose transcript is in the fixture) and `~/.CLAUDE` (case alias). Last,
    /// `~/.claude/accounts/work`, inside `~/.claude` but outside its scan
    /// roots, is accepted with its own card. The config-dir setter is judged against the fixture home, as
    /// production judges it against the process home. The dashboard path
    /// (`model_report::run`) has no exclusion and is the control.
    #[test]
    fn registering_the_default_claude_dir_as_extra_keeps_the_primary_window() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let default_dir = home.join(".claude");
        write_session(&default_dir, "primary", PRIMARY_OUTPUT);
        let app_support = home.join("Library/Application Support/Claude");
        write_session(
            &app_support.join("local-agent-mode-sessions/w/s/local_1/.claude"),
            "cowork",
            COWORK_OUTPUT,
        );
        let expected = PRIMARY_OUTPUT + COWORK_OUTPUT;
        let context = crate::LocalSourceContext::for_home(home.clone());

        // The production entry the window card uses, cache included.
        let card = |account: &Account| {
            reset_cache_only();
            cached(&context, account, WINDOW_FROM, WINDOW_UNTIL).map(|v| output_tokens(&v))
        };
        let dashboard = || {
            crate::model_report::run(&context, "").expect("model report")["totalOutput"]
                .as_i64()
                .unwrap()
        };
        let set_dirs = |dirs: &[&PathBuf]| {
            crate::claude_config_dirs::set_from_json_with_home(
                &serde_json::json!(dirs
                    .iter()
                    .map(|d| d.display().to_string())
                    .collect::<Vec<_>>())
                .to_string(),
                Some(&home),
            )
            .unwrap();
        };
        let set_roots = |dirs: &[&PathBuf]| {
            crate::extra_scan_paths::set_from_json(
                &serde_json::json!({ "claude": claude_roots(dirs) }).to_string(),
            )
            .unwrap();
        };

        // Control: nothing registered.
        let control = (card(&PRIMARY), dashboard());
        assert_eq!(control.0, Ok(expected), "control: primary window");
        assert_eq!(control.1, expected, "control: dashboard");

        let upper = home.join(".CLAUDE");
        let projects = default_dir.join("projects");
        let inside_projects = projects.join("x");
        for (name, extra, dirs, roots) in [
            ("a: ~/.claude, config dirs only", &default_dir, true, false),
            (
                "b: ~/.claude, scan registry only",
                &default_dir,
                false,
                true,
            ),
            ("c: ~/.claude, both (production)", &default_dir, true, true),
            ("<home>, both", &home, true, true),
            ("~/.claude/projects, both", &projects, true, true),
            ("~/.claude/projects/x, both", &inside_projects, true, true),
            (
                "~/Library/Application Support/Claude, both",
                &app_support,
                true,
                true,
            ),
            ("~/.CLAUDE, both", &upper, true, true),
        ] {
            reset_registries();
            if dirs {
                set_dirs(&[extra]);
            }
            if roots {
                set_roots(&[extra]);
            }
            let primary = card(&PRIMARY);
            assert_eq!(dashboard(), expected, "{name}: dashboard total");
            assert_eq!(
                primary,
                Ok(expected),
                "{name}: the primary window lost its own transcripts"
            );
            if dirs {
                assert!(
                    crate::claude_config_dirs::snapshot().is_empty(),
                    "{name}: the config-dir setter registered a primary scan root"
                );
            }
        }

        // Accepted: a folder inside `~/.claude` but outside its scan roots
        // (the primary scans only those roots). It registers, keeps its own
        // card, and the primary window keeps its total.
        let inside_claude = default_dir.join("accounts/work");
        write_session(&inside_claude, "inside", INSIDE_CLAUDE_OUTPUT);
        reset_registries();
        set_dirs(&[&inside_claude]);
        set_roots(&[&inside_claude]);
        assert_eq!(
            crate::claude_config_dirs::snapshot(),
            vec![inside_claude.display().to_string()],
            "~/.claude/accounts/work was refused, though no primary scan root holds it"
        );
        assert_eq!(
            card(&PRIMARY),
            Ok(expected),
            "~/.claude/accounts/work: primary window"
        );
        assert_eq!(
            card(&Some(inside_claude.display().to_string())),
            Ok(INSIDE_CLAUDE_OUTPUT),
            "~/.claude/accounts/work: its own card"
        );
        assert_eq!(
            dashboard(),
            expected + INSIDE_CLAUDE_OUTPUT,
            "~/.claude/accounts/work: dashboard"
        );
        reset_registries();
    }

    /// The primary also scans the `.cc-mirror` variants' project roots and
    /// counts their usage in its window. A variant whose `variant.json` names
    /// a `configDir` elsewhere is an ordinary Claude config dir: registered as
    /// an extra account it is NOT refused, and the primary window hands its
    /// usage to the account's own card (issue #258; `account_fixture`
    /// registers `work-d`, which variant `v1` names), the dashboard total
    /// staying put. A default-layout variant (`~/.cc-mirror/<v>/config`) is a
    /// primary scan root: its rows are tagged by path, no card of its own
    /// could show them (measured: the account's card reads 0), so the setter
    /// refuses it, and the `primary_exclusions` guard keeps it counted even if
    /// the scan registry alone holds it.
    #[test]
    fn a_cc_mirror_config_dir_is_an_account_only_when_it_lives_elsewhere() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        write_session(&home.join(".claude"), "primary", PRIMARY_OUTPUT);
        let default_layout = home.join(".cc-mirror/v2/config");
        let elsewhere = home.join("mirror-v1");
        for (variant, config) in [("v1", Some(&elsewhere)), ("v2", None)] {
            let variant_dir = home.join(".cc-mirror").join(variant);
            std::fs::create_dir_all(&variant_dir).unwrap();
            let body = config
                .map(|c| serde_json::json!({ "configDir": c.to_string_lossy() }))
                .unwrap_or_else(|| serde_json::json!({}));
            std::fs::write(variant_dir.join("variant.json"), body.to_string()).unwrap();
        }
        write_session(&elsewhere, "mirror-v1", 40);
        write_session(&default_layout, "mirror-v2", 25);
        let total = PRIMARY_OUTPUT + 40 + 25;
        let context = crate::LocalSourceContext::for_home(home.clone());
        let card = |account: &Account| {
            reset_cache_only();
            cached(&context, account, WINDOW_FROM, WINDOW_UNTIL).map(|v| output_tokens(&v))
        };
        let dashboard = || {
            crate::model_report::run(&context, "").expect("model report")["totalOutput"]
                .as_i64()
                .unwrap()
        };
        let set_dirs = |dir: &PathBuf| {
            crate::claude_config_dirs::set_from_json_with_home(
                &serde_json::json!([dir.display().to_string()]).to_string(),
                Some(&home),
            )
            .unwrap()
        };
        let set_roots = |dir: &PathBuf| {
            crate::extra_scan_paths::set_from_json(
                &serde_json::json!({ "claude": claude_roots(&[dir]) }).to_string(),
            )
            .unwrap();
        };
        assert_eq!(
            card(&PRIMARY),
            Ok(total),
            "control: the primary counts the mirrors"
        );

        // Elsewhere: accepted, moves to its own card, the total stays.
        reset_registries();
        assert_eq!(set_dirs(&elsewhere)["registeredCount"], 1, "refused");
        set_roots(&elsewhere);
        assert_eq!(card(&PRIMARY), Ok(total - 40), "elsewhere: primary window");
        assert_eq!(
            card(&Some(elsewhere.display().to_string())),
            Ok(40),
            "elsewhere: own card"
        );
        assert_eq!(dashboard(), total, "elsewhere: dashboard total");

        // Default layout: refused by the setter, and kept counted by the
        // primary even when only the scan registry holds it.
        reset_registries();
        let refused = set_dirs(&default_layout);
        assert_eq!(
            refused["registeredCount"], 0,
            "the default-layout variant was registered"
        );
        assert_eq!(refused["rejected"][0]["reason"], "primary_scan_root");
        set_roots(&default_layout);
        assert_eq!(card(&PRIMARY), Ok(total), "default layout: primary window");
        assert_eq!(dashboard(), total, "default layout: dashboard total");
        reset_registries();
    }

    /// `primary_exclusions` reads the primary's roots once for the whole list
    /// of registered paths, not once per path. Mutation: build `PrimaryRoots`
    /// inside the `retain` closure.
    #[test]
    fn primary_exclusions_build_the_primary_roots_once() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let extras: Vec<PathBuf> = (0..4).map(|i| home.join(format!("extra-{i}"))).collect();
        install(&claude_roots(&extras.iter().collect::<Vec<_>>()), &[]);
        let calls = || crate::claude_config_dirs::FOR_HOME_CALLS.with(std::cell::Cell::get);
        let before = calls();
        let excluded = primary_exclusions(Some(&home));
        assert_eq!(excluded.len(), 8, "fixture: eight registered roots");
        assert_eq!(calls() - before, 1, "built once for eight paths");
        // Control: nothing registered, nothing read.
        reset_registries();
        let before = calls();
        assert!(primary_exclusions(Some(&home)).is_empty());
        assert_eq!(calls() - before, 0);
        reset_registries();
    }

    fn reset_cache_only() {
        crate::WINDOW_USAGE_CACHE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}
