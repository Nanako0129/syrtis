//! Consumer-side wiring for tokscale-core's per-client `extra_scan_paths`
//! mechanism (`vendor/tokscale-core/src/scanner.rs:60`). The mechanism itself
//! is fully implemented upstream — canonical dedup, home-escape warning, and
//! merge into the scan queue all already exist; this module only holds the
//! process-wide registry `tb_set_extra_scan_paths` writes into and that
//! `LocalSourceContext` reads on every report/parse call.
//!
//! A `RwLock`, not an env var: Syrtis keeps a resident rayon scan pool
//! (`RAYON_INIT` in lib.rs), and Settings changes must take effect without an
//! app restart. `std::env::set_var` is unsafe in a multi-threaded process for
//! exactly that reason (see D1 in the extra-root plan).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{LazyLock, RwLock};

/// Process-wide extra scan roots, keyed by public client id (e.g. `"claude"`).
/// Empty by default, matching `ScannerSettings::default()` — a process that
/// never calls the setter behaves exactly as before this feature existed.
static EXTRA_SCAN_PATHS: LazyLock<RwLock<BTreeMap<String, Vec<PathBuf>>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));

/// Snapshot for building a `ScannerSettings` on each report/parse call.
pub(crate) fn snapshot() -> BTreeMap<String, Vec<PathBuf>> {
    EXTRA_SCAN_PATHS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Public client ids this consumer actually wires an extra-scan-path
/// registry entry for. Upstream's `extra_scan_paths_for()`
/// (`vendor/tokscale-core/src/scanner.rs:412`) silently drops unknown ids and
/// `supports_extra_dir_scanning()` (`:743`) excludes a few known ones — but
/// that upstream filtering happens deep inside the scan, long after this
/// setter has already reported success. A client id that gets this far and
/// is not actually scannable must be rejected here, not accepted into a
/// registry entry that will never contribute to a report.
///
/// Extend this list when Syrtis wires extra-root support for another
/// client.
const SUPPORTED_CLIENTS: &[&str] = &["claude"];

/// Clear the registry. Test seam: the statics are process-wide, so a test that
/// leaves one populated changes the next test's answer.
#[cfg(test)]
pub(crate) fn reset_for_test() {
    *EXTRA_SCAN_PATHS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = BTreeMap::new();
}

/// What a configured path can become, which is not the same question as
/// whether it is usable right now.
enum PathShape {
    /// Belongs in the registry. `Some(reason)` when it isn't readable at this
    /// moment but plausibly will be later.
    Registrable(Option<String>),
    /// Can never contribute a transcript, so registering it would either do
    /// nothing or actively ingest the wrong data.
    Unusable(String),
}

/// Split "unreadable right now" from "wrong shape forever".
///
/// Retaining an absent root is the whole point of registering optimistically:
/// an unmounted volume or a config dir Claude hasn't populated yet becomes
/// valid on its own, and the next scan picks it up. A path that exists but is
/// not a directory never becomes one, and retaining it is worse than useless —
/// upstream's `scan_directory` (`vendor/tokscale-core/src/scanner.rs:278`)
/// only checks `exists()` before handing the root to `WalkDir`, which yields a
/// regular file as its own entry. A `*.jsonl` file passed here would therefore
/// be reported as merely `unreadable` while silently contributing its contents
/// to the totals. Empty and relative paths are rejected for the same reason:
/// `ctb.h` documents absolute directories, and a relative path resolves
/// against whatever the process CWD happens to be.
fn classify(path: &std::path::Path) -> PathShape {
    if path.as_os_str().is_empty() {
        return PathShape::Unusable("empty path".to_string());
    }
    if !path.is_absolute() {
        return PathShape::Unusable("path is not absolute".to_string());
    }
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => match std::fs::read_dir(path) {
            Ok(_) => PathShape::Registrable(None),
            // A directory that exists but can't be read right now (permissions,
            // a stalled network mount) may recover, so keep it.
            Err(e) => PathShape::Registrable(Some(e.to_string())),
        },
        Ok(_) => PathShape::Unusable("path is not a directory".to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            PathShape::Registrable(Some(e.to_string()))
        }
        Err(e) => PathShape::Registrable(Some(e.to_string())),
    }
}

/// Replace the whole registry from a JSON object of
/// `{"<client-id>": ["<path>", ...]}`. Full-replace, not merge: calling with
/// `{}` clears every configured root (the Settings rollback path).
///
/// Two things drop a path, for different reasons. An unsupported client id is
/// dropped because upstream would silently discard it deep inside the scan
/// (see `SUPPORTED_CLIENTS`). A path `classify()` calls `Unusable` is dropped
/// because it can never be a scan root at all. Everything else is registered,
/// including a directory that cannot be read at this moment: an unmounted
/// volume or a config dir Claude has not populated yet becomes valid on its
/// own, upstream's scan already skips a root that is missing at scan time, and
/// dropping it here would orphan the user's setting until an app restart —
/// nothing re-registers an unchanged UserDefaults value.
pub(crate) fn set_from_json(raw: &str) -> Result<serde_json::Value, String> {
    let input: BTreeMap<String, Vec<String>> =
        serde_json::from_str(raw).map_err(|e| format!("invalid extra scan paths JSON: {}", e))?;

    let mut registered: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    let mut registered_count = 0usize;
    let mut unreadable: Vec<serde_json::Value> = Vec::new();
    let mut rejected: Vec<serde_json::Value> = Vec::new();

    for (client_id, paths) in input {
        if !SUPPORTED_CLIENTS.contains(&client_id.as_str()) {
            for raw_path in paths {
                rejected.push(serde_json::json!({
                    "client": client_id,
                    "path": raw_path,
                    "reason": "unsupported client",
                }));
            }
            continue;
        }

        let mut client_paths = Vec::new();
        for raw_path in paths {
            let path = PathBuf::from(&raw_path);
            match classify(&path) {
                PathShape::Registrable(note) => {
                    if let Some(reason) = note {
                        unreadable.push(serde_json::json!({
                            "client": client_id,
                            "path": raw_path,
                            "reason": reason,
                        }));
                    }
                    client_paths.push(path);
                    registered_count += 1;
                }
                PathShape::Unusable(reason) => rejected.push(serde_json::json!({
                    "client": client_id,
                    "path": raw_path,
                    "reason": reason,
                })),
            }
        }
        if !client_paths.is_empty() {
            registered.insert(client_id, client_paths);
        }
    }

    *EXTRA_SCAN_PATHS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = registered;

    Ok(serde_json::json!({
        "registeredCount": registered_count,
        "unreadable": unreadable,
        "rejected": rejected,
    }))
}

/// Guards every test that touches `EXTRA_SCAN_PATHS` behind one process-wide
/// mutex so concurrent `cargo test` threads don't stomp each other's writes —
/// the static is process state, not per-test state. It lives outside the tests
/// module because `lib.rs` has a test that drives the registry through the FFI
/// entry point and has to serialize against these.
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LocalSourceContext;
    use std::fs;
    use std::path::Path;

    fn reset() {
        *EXTRA_SCAN_PATHS
            .write()
            .unwrap_or_else(|p| p.into_inner()) = BTreeMap::new();
    }

    fn write_claude_session(root: &Path, session_name: &str, content: &str) {
        let dir = root.join("claude-project");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{session_name}.jsonl")), content).unwrap();
    }

    /// One assistant turn with distinct message/request ids -> known token total.
    fn turn(msg_id: &str, req_id: &str, input: i64, output: i64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"2026-01-01T00:00:00.000Z","requestId":"{req_id}","message":{{"id":"{msg_id}","model":"claude-3-5-sonnet","usage":{{"input_tokens":{input},"output_tokens":{output}}}}}}}"#
        )
    }

    fn total_claude_tokens(context: &LocalSourceContext) -> i64 {
        let options = context.parse_options(None, Some(vec!["claude".to_string()]));
        let parsed = tokscale_core::parse_local_clients(options).unwrap();
        parsed
            .messages
            .iter()
            .map(|m| m.input + m.output + m.cache_read + m.cache_write + m.reasoning)
            .sum()
    }

    fn context_for(home: &Path) -> LocalSourceContext {
        LocalSourceContext {
            home_dir: Some(home.to_path_buf()),
            use_env_roots: true,
        }
    }

    fn set_claude_roots(paths: &[&Path]) {
        set_claude_roots_result(paths);
    }

    fn set_claude_roots_result(paths: &[&Path]) -> serde_json::Value {
        let list: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        let payload = serde_json::json!({ "claude": list }).to_string();
        set_from_json(&payload).unwrap()
    }

    // A1: two temp roots, id-disjoint content. Unset -> only primary. Set -> sum.
    #[test]
    fn a1_extra_root_adds_to_primary_when_ids_are_disjoint() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset();
        let primary = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        write_claude_session(
            &primary.path().join(".claude/projects"),
            "primary",
            &turn("msg_primary", "req_primary", 100, 50),
        );
        let extra_root = extra.path().join("projects");
        write_claude_session(&extra_root, "extra", &turn("msg_extra", "req_extra", 200, 30));

        let context = context_for(primary.path());
        assert_eq!(total_claude_tokens(&context), 150, "primary alone before setter");

        set_claude_roots(&[&extra_root]);
        assert_eq!(
            total_claude_tokens(&context),
            150 + 230,
            "primary + extra root after setter"
        );
        reset();
    }

    // A2: mutation — change the extra fixture's token count and prove the report
    // total tracks it. Demonstrates "changes before red, after green" instead of
    // asserting a single static number.
    #[test]
    fn a2_mutating_extra_root_fixture_changes_the_reported_total() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset();
        let primary = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let extra_root = extra.path().join("projects");
        write_claude_session(&extra_root, "extra", &turn("msg_a2", "req_a2", 100, 20));
        write_claude_session(&primary.path().join(".claude/projects"), "primary",
            &turn("msg_a2_primary", "req_a2_primary", 5, 5));

        let context = context_for(primary.path());
        set_claude_roots(&[&extra_root]);
        let before = total_claude_tokens(&context);
        assert_eq!(before, 10 + 120);

        // Mutate the fixture in place: same message id would dedup, so use a
        // fresh id/requestId pair — this is a genuinely new message, not an
        // edit of the old one, matching how a real second CLAUDE_CONFIG_DIR
        // session would append.
        write_claude_session(
            &extra_root,
            "extra2",
            &turn("msg_a2_new", "req_a2_new", 900, 900),
        );
        let after = total_claude_tokens(&context);
        assert_ne!(before, after, "mutated fixture must change the total (red would be: unchanged)");
        assert_eq!(after, before + 1800);
        reset();
    }

    // A3a: extra root duplicates the primary's dedup key -> total unchanged.
    #[test]
    fn a3a_duplicate_dedup_key_across_roots_does_not_double_count() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset();
        let primary = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let shared = turn("msg_shared", "req_shared", 100, 50);
        write_claude_session(&primary.path().join(".claude/projects"), "primary", &shared);
        let extra_root = extra.path().join("projects");
        // Same message/request id, different file/root -> same dedup key.
        write_claude_session(&extra_root, "duplicate", &shared);

        let context = context_for(primary.path());
        let before = total_claude_tokens(&context);
        set_claude_roots(&[&extra_root]);
        let after = total_claude_tokens(&context);
        assert_eq!(before, after, "duplicate dedup key must not double the total");
        assert_eq!(after, 150);
        reset();
    }

    // A3b: the exact same path listed twice in the registry doesn't double count
    // either — push_unique_scan_task canonical-dedups scan roots themselves.
    #[test]
    fn a3b_same_path_listed_twice_does_not_double_count() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset();
        let primary = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let extra_root = extra.path().join("projects");
        write_claude_session(&extra_root, "extra", &turn("msg_dup_path", "req_dup_path", 40, 10));

        let context = context_for(primary.path());
        set_claude_roots(&[&extra_root, &extra_root]);
        assert_eq!(total_claude_tokens(&context), 50);
        reset();
    }

    // A4: nonexistent / non-directory paths are still REGISTERED (reported as
    // unreadable, not rejected) and don't panic or block the rest of the
    // registry from scanning. This is the envelope-shape half of defect 1's
    // fix; a4c below proves the behavioral half (a currently-unreadable root
    // is picked up by a later scan without calling the setter again).
    #[test]
    fn a4_invalid_paths_are_registered_as_unreadable_without_panicking() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset();
        let primary = tempfile::tempdir().unwrap();
        let valid = tempfile::tempdir().unwrap();
        let valid_root = valid.path().join("projects");
        write_claude_session(&valid_root, "valid", &turn("msg_valid", "req_valid", 10, 10));

        let missing = primary.path().join("does-not-exist");
        let a_file = primary.path().join("a-file.txt");
        fs::write(&a_file, b"not a directory").unwrap();

        let payload = serde_json::json!({
            "claude": [
                valid_root.display().to_string(),
                missing.display().to_string(),
                a_file.display().to_string(),
            ]
        })
        .to_string();
        let result = set_from_json(&payload).unwrap();

        assert_eq!(
            result["registeredCount"], 2,
            "the valid root and the not-yet-existing one are registered; a path \
             that exists but is not a directory is not"
        );
        let unreadable_paths: Vec<String> = result["unreadable"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["path"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            unreadable_paths,
            vec![missing.display().to_string()],
            "only the absent root is retryable — it can still become a directory"
        );
        let rejected_paths: Vec<String> = result["rejected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["path"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            rejected_paths,
            vec![a_file.display().to_string()],
            "an existing non-directory never becomes one, so it is rejected \
             rather than retried"
        );

        // The valid root still scans normally alongside the primary.
        let context = context_for(primary.path());
        assert_eq!(total_claude_tokens(&context), 20);
        reset();
    }

    // A4d: a transcript FILE handed in as a root must be rejected outright, not
    // merely reported unreadable. Upstream's scan_directory only checks
    // exists() before WalkDir, and WalkDir yields a regular file as its own
    // entry, so a registered *.jsonl path silently contributes its contents to
    // the totals while the envelope claims it could not be read. A4's
    // non-directory fixture is a .txt and cannot catch this.
    #[test]
    fn a4d_a_transcript_file_as_root_is_rejected_and_contributes_nothing() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset();
        let primary = tempfile::tempdir().unwrap();
        let stray = tempfile::tempdir().unwrap();
        // A real, parseable Claude transcript — not a dummy file.
        let file_root = stray.path().join("session.jsonl");
        fs::write(&file_root, turn("msg_stray", "req_stray", 500, 500)).unwrap();

        let context = context_for(primary.path());
        let baseline = total_claude_tokens(&context);

        let result = set_claude_roots_result(&[&file_root]);
        assert_eq!(result["registeredCount"], 0, "a file root is never registered");
        assert_eq!(result["unreadable"].as_array().unwrap().len(), 0);
        assert_eq!(
            result["rejected"].as_array().unwrap().len(),
            1,
            "and it is reported as rejected, not as a retryable unreadable path"
        );

        assert_eq!(
            total_claude_tokens(&context),
            baseline,
            "its 1000 tokens must not reach the totals"
        );
        reset();
    }

    // A4c (defect 1 regression): a root that doesn't exist yet at setter time
    // must still be scanned once it starts existing, WITHOUT calling the
    // setter again. This is exactly the unmounted-volume / not-yet-created
    // config dir scenario: the setter registers the root as unreadable, the
    // volume mounts / the dir gets created out-of-band, and the very next
    // scan must pick it up purely because it's already in the registry.
    #[test]
    fn a4c_unreadable_root_at_register_time_is_picked_up_by_a_later_scan() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset();
        let primary = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        // Not created yet: `extra_root` does not exist on disk at register time.
        let extra_root = extra.path().join("not-yet-mounted").join("projects");
        assert!(!extra_root.exists());

        let context = context_for(primary.path());
        let result = set_claude_roots_result(&[&extra_root]);
        assert_eq!(result["registeredCount"], 1, "the not-yet-existing root is still registered");
        assert_eq!(
            result["unreadable"].as_array().unwrap().len(),
            1,
            "and reported as currently unreadable"
        );
        assert_eq!(total_claude_tokens(&context), 0, "nothing to scan yet");

        // The root "mounts": the directory and a session file now exist.
        // Crucially, the setter is NOT called again here.
        write_claude_session(&extra_root, "now-mounted", &turn("msg_a4c", "req_a4c", 60, 40));

        assert_eq!(
            total_claude_tokens(&context),
            100,
            "a later scan must pick up the root purely because it was already \
             registered — no setter re-call, no app restart"
        );
        reset();
    }

    // A4b: malformed JSON returns a structured error, not a panic.
    #[test]
    fn a4b_invalid_json_returns_error_result() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset();
        let err = set_from_json("{not json").unwrap_err();
        assert!(err.contains("invalid extra scan paths JSON"), "got: {err}");
        reset();
    }

    // A5: cache-invalidation contract — the change token used by tb_graph's
    // cache (`local_source_change_token`) must differ once an extra root with
    // new files is registered, so a warm graph cache doesn't hide the new data.
    #[test]
    fn a5_change_token_reflects_newly_registered_extra_root() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset();
        let primary = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        write_claude_session(&primary.path().join(".claude/projects"), "primary",
            &turn("msg_a5", "req_a5", 5, 5));
        let extra_root = extra.path().join("projects");
        write_claude_session(&extra_root, "extra", &turn("msg_a5_extra", "req_a5_extra", 5, 5));

        let context = context_for(primary.path());
        let before_token =
            tokscale_core::local_source_change_token(&context.parse_options(None, None)).unwrap();

        set_claude_roots(&[&extra_root]);
        let after_token =
            tokscale_core::local_source_change_token(&context.parse_options(None, None)).unwrap();

        assert_ne!(before_token, after_token, "change token must move once a new root's files enter the scan set");
        reset();
    }

    // A6: never calling the setter must behave exactly as before this feature —
    // the registry defaults to empty, so scanner_settings.extra_scan_paths is
    // the same empty BTreeMap ScannerSettings::default() produces.
    #[test]
    fn a6_never_calling_setter_matches_pre_feature_behavior() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset(); // simulate "never called" - registry untouched since process start
        let primary = tempfile::tempdir().unwrap();
        let context = context_for(primary.path());
        let options = context.parse_options(None, None);
        assert_eq!(
            options.scanner_settings.extra_scan_paths,
            tokscale_core::scanner::ScannerSettings::default().extra_scan_paths
        );
        assert!(options.scanner_settings.extra_scan_paths.is_empty());
    }

    // A8: extra config dir carries both `projects` and `transcripts` — both
    // must contribute (id-disjoint), proving D2's two-subpath expansion.
    #[test]
    fn a8_both_projects_and_transcripts_subpaths_contribute() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset();
        let primary = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let projects_root = extra.path().join("projects");
        let transcripts_root = extra.path().join("transcripts");
        write_claude_session(&projects_root, "projects-session", &turn("msg_proj", "req_proj", 40, 10));
        write_claude_session(&transcripts_root, "transcripts-session", &turn("msg_tr", "req_tr", 25, 5));

        let context = context_for(primary.path());
        set_claude_roots(&[&projects_root, &transcripts_root]);
        assert_eq!(total_claude_tokens(&context), 50 + 30);
        reset();
    }

    // A9 (defect 2 regression): a client id outside SUPPORTED_CLIENTS is
    // rejected wholesale, along with its paths — not silently accepted into
    // a registry entry that upstream would never scan anyway.
    #[test]
    fn a9_unsupported_client_id_is_rejected_and_never_registered() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset();
        let valid = tempfile::tempdir().unwrap();
        let valid_root = valid.path().join("projects");
        write_claude_session(&valid_root, "valid", &turn("msg_a9", "req_a9", 10, 10));

        // "crush" is excluded by upstream's supports_extra_dir_scanning() —
        // any client id not in SUPPORTED_CLIENTS exercises the same path.
        let payload = serde_json::json!({ "crush": [valid_root.display().to_string()] }).to_string();
        let result = set_from_json(&payload).unwrap();

        assert_eq!(result["registeredCount"], 0, "unsupported client contributes nothing to the registry");
        assert_eq!(result["unreadable"].as_array().unwrap().len(), 0);
        let rejected = result["rejected"].as_array().unwrap();
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0]["client"], "crush");
        assert_eq!(rejected[0]["path"], valid_root.display().to_string());
        assert_eq!(rejected[0]["reason"], "unsupported client");

        assert!(
            snapshot().is_empty(),
            "the registry must not carry an entry for an unsupported client"
        );
        reset();
    }
}
