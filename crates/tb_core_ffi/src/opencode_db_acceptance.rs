//! A user-configured OpenCode database (`OPENCODE_DB`, or
//! `scanner.opencodeDbPaths` in the tokscale settings file) must reach every
//! source-scanning FFI entry point, because all of them build their options
//! through `LocalSourceContext::scanner_settings`.
//!
//! Each scenario runs in an env-cleared child process: the parent re-executes
//! this test binary with only HOME / TOKSCALE_CONFIG_DIR (and, per scenario,
//! OPENCODE_DB) set, so neither the developer's real settings nor environment
//! can leak in. The fixture DB sits in `<root>/external`, away from the
//! child's default OpenCode dir (empty), so the numbers are exact.

use std::ffi::{c_char, CStr, CString};
use std::path::{Path, PathBuf};
use std::process::Command;

const MODEL: &str = "oc-fixture-model";
const OUTPUT: i64 = 4321;
const WINDOW_FROM: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z
const WINDOW_UNTIL: i64 = 1_767_312_000_000; // 2026-01-02T00:00:00Z
const ROOT_VAR: &str = "TB_OPENCODE_ACCEPT_ROOT";
const ENV_VAR: &str = "TB_OPENCODE_ACCEPT_SET_ENV";

/// `None` in the parent. In the child, the fixture root.
fn child_root() -> Option<PathBuf> {
    std::env::var_os(ROOT_VAR).map(PathBuf::from)
}

/// Re-run `test` in an env-cleared child. `with_env` passes the DB path to
/// the child as `OPENCODE_DB` (the child creates the DB, so the parent only
/// names where it will be).
fn run_in_child(test: &str, label: &str, with_env: bool) {
    let root = tempfile::Builder::new().prefix(label).tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", &format!("opencode_db_acceptance::{test}"), "--nocapture"])
        .env_clear()
        .env(ROOT_VAR, root.path())
        .env("HOME", &home)
        .env("TOKSCALE_CONFIG_DIR", root.path().join("tokscale-config"))
        .env("TOKSCALE_PRICING_CACHE_ONLY", "1");
    if with_env {
        cmd.env(ENV_VAR, "1")
            .env("OPENCODE_DB", root.path().join("external").join("opencode.db"));
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("1 passed"),
        "child failed: {}\n{}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write_external_db(root: &Path) -> PathBuf {
    let dir = root.join("external");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("opencode.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, data TEXT NOT NULL);
         CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL);",
    )
    .unwrap();
    conn.execute("INSERT INTO session VALUES ('s1', '/work')", []).unwrap();
    let data = format!(
        r#"{{"id":"m1","role":"assistant","modelID":"{MODEL}","providerID":"anthropic","mode":"build","cost":0.0,
        "tokens":{{"input":100,"output":{OUTPUT},"reasoning":0,"cache":{{"read":0,"write":0}}}},
        "time":{{"created":1767225600000.0,"completed":1767225601000.0}}}}"#
    );
    conn.execute("INSERT INTO message VALUES ('m1', 's1', ?1)", [data]).unwrap();
    db
}

fn write_settings(root: &Path, body: serde_json::Value) {
    let config = root.join("tokscale-config");
    std::fs::create_dir_all(&config).unwrap();
    let settings = serde_json::json!({"scanner": {"opencodeDbPaths": body}});
    std::fs::write(config.join("settings.json"), settings.to_string()).unwrap();
}

unsafe fn take(p: *mut c_char) -> serde_json::Value {
    let text = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { crate::tb_free(p) };
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

/// Every payload the scanning entry points return, keyed by entry name.
fn all_entries() -> Vec<(&'static str, serde_json::Value)> {
    let nul = std::ptr::null();
    let opencode = CString::new("opencode").unwrap();
    unsafe {
        vec![
            ("tb_graph", take(crate::tb_graph(nul))),
            ("tb_refresh_graph", take(crate::tb_refresh_graph(nul))),
            ("tb_model_report", take(crate::tb_model_report(nul))),
            ("tb_hourly_report", take(crate::tb_hourly_report(nul, opencode.as_ptr()))),
            ("tb_agents_report", take(crate::tb_agents_report(nul, opencode.as_ptr()))),
            ("tb_window_usage", take(crate::tb_window_usage(nul, WINDOW_FROM, WINDOW_UNTIL))),
        ]
    }
}

/// Whether any JSON number node equals `n`.
fn has_int(value: &serde_json::Value, n: i64) -> bool {
    match value {
        serde_json::Value::Number(x) => x.as_i64() == Some(n),
        serde_json::Value::Array(items) => items.iter().any(|v| has_int(v, n)),
        serde_json::Value::Object(map) => map.values().any(|v| has_int(v, n)),
        _ => false,
    }
}

/// Entries whose payload scanned the DB: it names the fixture model (agent
/// for the Agents report) and carries its output tokens exactly once -- the
/// fixture output appears and twice that (a double count) does not.
fn entries_seeing_the_db() -> Vec<&'static str> {
    let mut seeing: Vec<&'static str> = all_entries()
        .into_iter()
        .inspect(|(name, payload)| assert_eq!(payload["ok"], true, "{name}: {payload}"))
        .filter(|(name, payload)| {
            // The Agents report buckets by agent (OpenCode's `mode`), not model.
            let marker = if *name == "tb_agents_report" { "build" } else { MODEL };
            assert!(!has_int(payload, 2 * OUTPUT), "{name} double counted: {payload}");
            payload.to_string().to_lowercase().contains(marker) && has_int(payload, OUTPUT)
        })
        .map(|(name, _)| name)
        .collect();
    // tb_probe parses messages (the usage tail's path): one message when seen.
    let probe = unsafe { take(crate::tb_probe()) };
    assert_eq!(probe["ok"], true, "{probe}");
    if probe["messages"] == 1 {
        seeing.push("tb_probe");
    } else {
        assert_eq!(probe["messages"], 0, "{probe}");
    }
    seeing
}

const EVERY_ENTRY: [&str; 7] = [
    "tb_graph",
    "tb_refresh_graph",
    "tb_model_report",
    "tb_hourly_report",
    "tb_agents_report",
    "tb_window_usage",
    "tb_probe",
];

#[test]
fn opencode_db_env_reaches_every_entry() {
    let Some(root) = child_root() else {
        return run_in_child("opencode_db_env_reaches_every_entry", "oc-env", true);
    };
    write_external_db(&root);
    assert!(std::env::var_os(ENV_VAR).is_some());
    assert_eq!(entries_seeing_the_db(), EVERY_ENTRY);
}

#[test]
fn settings_file_opencode_db_paths_reach_every_entry() {
    let Some(root) = child_root() else {
        return run_in_child("settings_file_opencode_db_paths_reach_every_entry", "oc-file", false);
    };
    let db = write_external_db(&root);
    write_settings(&root, serde_json::json!([db]));
    assert_eq!(entries_seeing_the_db(), EVERY_ENTRY);
}

/// `~/` in the settings file resolves against the process home.
#[test]
fn a_tilde_path_in_the_settings_file_reaches_every_entry() {
    let Some(root) = child_root() else {
        return run_in_child("a_tilde_path_in_the_settings_file_reaches_every_entry", "oc-tilde", false);
    };
    // The DB lives under HOME (but outside the default ~/.local/share/opencode).
    let db = write_external_db(&root);
    let in_home = root.join("home").join("elsewhere");
    std::fs::create_dir_all(&in_home).unwrap();
    std::fs::rename(&db, in_home.join("opencode.db")).unwrap();
    write_settings(&root, serde_json::json!(["~/elsewhere/opencode.db"]));
    assert_eq!(entries_seeing_the_db(), EVERY_ENTRY);
}

/// Control: the DB exists but nothing points at it. Behavior is exactly the
/// pre-fix default.
#[test]
fn without_configuration_the_external_db_stays_unscanned() {
    let Some(root) = child_root() else {
        return run_in_child("without_configuration_the_external_db_stays_unscanned", "oc-none", false);
    };
    write_external_db(&root);
    assert_eq!(entries_seeing_the_db(), Vec::<&str>::new());
}

/// The same DB named by `OPENCODE_DB` and by settings.json is scanned once.
#[test]
fn the_same_db_from_env_and_file_counts_once() {
    let Some(root) = child_root() else {
        return run_in_child("the_same_db_from_env_and_file_counts_once", "oc-same", true);
    };
    let db = write_external_db(&root);
    write_settings(&root, serde_json::json!([db]));
    assert_eq!(entries_seeing_the_db(), EVERY_ENTRY);
}

/// settings.json edited after the first scan: an uncached entry sees it at
/// once; after a root setter (which clears the caches) every entry does.
#[test]
fn an_edited_settings_file_is_picked_up_after_the_setter() {
    let Some(root) = child_root() else {
        return run_in_child("an_edited_settings_file_is_picked_up_after_the_setter", "oc-recapture", false);
    };
    let db = write_external_db(&root);
    assert_eq!(entries_seeing_the_db(), Vec::<&str>::new());
    write_settings(&root, serde_json::json!([db]));
    let model = unsafe { take(crate::tb_model_report(std::ptr::null())) };
    assert!(has_int(&model, OUTPUT), "uncached scan re-reads the file: {model}");
    let none = CString::new("{}").unwrap();
    let set = unsafe { take(crate::tb_set_extra_scan_paths(none.as_ptr())) };
    assert_eq!(set["ok"], true, "{set}");
    assert_eq!(entries_seeing_the_db(), EVERY_ENTRY);
}

/// A configured path the engine would silently drop is reported by tb_probe,
/// with codes only.
#[test]
fn tb_probe_reports_rejected_paths() {
    let Some(root) = child_root() else {
        return run_in_child("tb_probe_reports_rejected_paths", "oc-probe", false);
    };
    let good = write_external_db(&root);
    let bad = root.join("external").join("mydb.db");
    write_settings(&root, serde_json::json!([bad, good]));
    let probe = unsafe { take(crate::tb_probe()) };
    assert_eq!(probe["ok"], true, "{probe}");
    let report = &probe["opencodeDb"];
    assert_eq!(report["settingsFile"], "ok");
    assert_eq!(report["handedToEngine"], 2);
    assert_eq!(report["issues"][0]["reason"], "badFileName");
    assert_eq!(report["issues"][0]["index"], 0);
    assert!(!report.to_string().contains("mydb"), "a path must not cross the ABI");
}
