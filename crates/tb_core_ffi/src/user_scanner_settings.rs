//! The user-owned part of the scan: where the user told tokscale an OpenCode
//! database lives outside opencode's default data dir.
//!
//! Two sources, the same two the tokscale CLI user has: the `scanner` key of
//! `<tokscale config dir>/settings.json` (`opencodeDbPaths`; the dir is the
//! engine's own `paths::get_config_dir()`: `TOKSCALE_CONFIG_DIR`, else
//! `~/.config/tokscale` on macOS), and the `OPENCODE_DB` environment variable
//! opencode itself reads. The engine honours neither on its own:
//! `ScannerSettings::opencode_db_paths` is an input and `OPENCODE_DB` is read
//! nowhere in it. Every source-scanning entry point builds its options through
//! `LocalSourceContext::scanner_settings`, so that is the one place the paths
//! enter. They are part of the options handed to the engine, so a change
//! re-keys its change token.
//!
//! When it runs: at each options build, so an edited `settings.json` is picked
//! up by the next scan (a cached graph / window answer may still be served for
//! up to `ONESHOT_MAX_AGE_SECS`, or until a root setter clears the caches).
//! `OPENCODE_DB` is read from the live environment too, but a GUI app's
//! environment is fixed at launch, so a changed value needs an app restart.
//! There is no process-context lock on macOS to keep this read out of.
//!
//! Only `opencodeDbPaths` is taken from the file: `extra_scan_paths` and
//! `excluded_scan_paths` stay owned by the root registry and Cursor takeover.
//!
//! Path syntax: surrounding whitespace and one pair of quotes are removed, and
//! a leading `~` / `~/` becomes the home directory (the one idiom a hand-edited
//! JSON file cannot get a shell to expand; `OPENCODE_DB` set from launchd gets
//! no shell either). `$VAR` is not expanded: neither opencode nor tokscale
//! documents it. A value that is not valid Unicode is passed through untouched.
//!
//! The engine drops a configured path silently when its file name is not
//! `opencode.db` / `opencode-<channel>.db` or the file is absent. Every
//! absolute path is handed to the engine regardless (the engine owns its
//! rules, so a rule change on a pin bump cannot be masked here); a local copy
//! of the name rule only produces the `badFileName` diagnostic. So that a
//! configured path never silently does nothing, each problem is recorded with
//! a fixed reason code (never the path: it names a user directory) and
//! surfaced in `tb_probe` under `opencodeDb`, and logged to stderr when the
//! set of problems changes. The probe shows what the last read found.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A settings file larger than this is not a hand-edited config; rejected.
const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;

#[derive(serde::Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct SettingsFile {
    scanner: ScannerKey,
}

#[derive(serde::Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ScannerKey {
    opencode_db_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Issue {
    /// `"env"` or `"file"`.
    pub source: &'static str,
    /// Zero-based position within that source's list (`env` has one entry).
    pub index: usize,
    /// `notAbsolute`: not handed to the engine. `badFileName`, `missing`:
    /// handed over anyway, the engine skips it (until the file exists, for
    /// `missing`).
    pub reason: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Report {
    /// `absent`, `ok`, `tooLarge`, `unreadable`, `invalidJson`.
    pub settings_file: &'static str,
    /// Distinct paths handed to the engine (it dedups again by canonical
    /// path). Includes paths that also carry a `badFileName`/`missing` issue.
    pub handed_to_engine: usize,
    pub issues: Vec<Issue>,
}

static LAST: Mutex<Option<Report>> = Mutex::new(None);
static LAST_LOGGED: Mutex<Option<Report>> = Mutex::new(None);

/// What the last [`opencode_db_paths`] call found, for `tb_probe`.
pub(crate) fn diagnostics() -> serde_json::Value {
    let report = LAST
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .unwrap_or(Report { settings_file: "absent", handed_to_engine: 0, issues: Vec::new() });
    serde_json::to_value(report).unwrap_or(serde_json::Value::Null)
}

/// The OpenCode DB paths the engine should scan, from the live config dir and
/// environment. `home` expands a leading `~`.
pub(crate) fn opencode_db_paths(home: Option<&Path>) -> Vec<PathBuf> {
    let (paths, report) = resolve(
        &tokscale_core::paths::get_config_dir().join("settings.json"),
        std::env::var_os("OPENCODE_DB"),
        home,
    );
    publish(&report);
    paths
}

fn publish(report: &Report) {
    *LAST.lock().unwrap_or_else(|p| p.into_inner()) = Some(report.clone());
    let problem = !report.issues.is_empty() || !matches!(report.settings_file, "absent" | "ok");
    let mut logged = LAST_LOGGED.lock().unwrap_or_else(|p| p.into_inner());
    if !problem {
        // A problem that comes back after a clean read logs again.
        *logged = None;
    } else if logged.as_ref() != Some(report) {
        eprintln!("tb_core_ffi: opencode db settings: {}", serde_json::json!(report));
        *logged = Some(report.clone());
    }
}

/// Engine rule (`is_opencode_db_filename`, private there): `opencode.db` or
/// `opencode-<channel>.db`.
fn is_opencode_db_filename(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".db") else {
        return false;
    };
    stem == "opencode"
        || stem.strip_prefix("opencode-").is_some_and(|channel| {
            !channel.is_empty()
                && channel
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
}

/// An absolute path to hand to the engine, plus the diagnostic (if any) that
/// the engine will probably skip it. Only `notAbsolute` withholds a path.
fn normalize_os(
    raw: &std::ffi::OsStr,
    home: Option<&Path>,
) -> Result<(PathBuf, Option<&'static str>), &'static str> {
    // Only valid Unicode is trimmed/unquoted/expanded; anything else is
    // passed through untouched.
    let path = match raw.to_str() {
        Some(text) => normalize(text, home),
        None => PathBuf::from(raw),
    };
    if !path.is_absolute() {
        return Err("notAbsolute");
    }
    let name_ok = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_opencode_db_filename);
    Ok((path, (!name_ok).then_some("badFileName")))
}

fn normalize(raw: &str, home: Option<&Path>) -> PathBuf {
    let mut text = raw.trim();
    for quote in ['"', '\''] {
        if let Some(inner) = text.strip_prefix(quote).and_then(|t| t.strip_suffix(quote)) {
            text = inner.trim();
            break;
        }
    }
    match (text, home) {
        ("~", Some(home)) => home.to_path_buf(),
        (_, Some(home)) if text.starts_with("~/") => home.join(&text[2..]),
        _ => PathBuf::from(text),
    }
}

/// One bounded read; a UTF-8 BOM (Windows PowerShell 5.1, some editors) is
/// stripped before parsing.
fn read_file_paths(file: &Path) -> (&'static str, Vec<String>) {
    let handle = match std::fs::File::open(file) {
        Ok(handle) => handle,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ("absent", Vec::new()),
        Err(_) => return ("unreadable", Vec::new()),
    };
    let mut bytes = Vec::new();
    if handle.take(MAX_SETTINGS_BYTES + 1).read_to_end(&mut bytes).is_err() {
        return ("unreadable", Vec::new());
    }
    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return ("tooLarge", Vec::new());
    }
    let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    match serde_json::from_slice::<SettingsFile>(body) {
        Ok(parsed) => ("ok", parsed.scanner.opencode_db_paths),
        Err(_) => ("invalidJson", Vec::new()),
    }
}

fn resolve(
    settings_file: &Path,
    opencode_db_env: Option<std::ffi::OsString>,
    home: Option<&Path>,
) -> (Vec<PathBuf>, Report) {
    let (state, from_file) = read_file_paths(settings_file);
    let mut paths = Vec::new();
    let mut issues = Vec::new();
    let mut seen = HashSet::new();
    let sources: [(&'static str, Vec<std::ffi::OsString>); 2] = [
        ("file", from_file.into_iter().map(Into::into).collect()),
        ("env", opencode_db_env.into_iter().collect()),
    ];
    for (source, raws) in sources {
        for (index, raw) in raws.iter().enumerate() {
            match normalize_os(raw, home) {
                Ok((path, name_issue)) => {
                    if let Some(reason) = name_issue {
                        issues.push(Issue { source, index, reason });
                    }
                    if !path.is_file() {
                        issues.push(Issue { source, index, reason: "missing" });
                    }
                    if seen.insert(path.clone()) {
                        paths.push(path);
                    }
                }
                Err(reason) => issues.push(Issue { source, index, reason }),
            }
        }
    }
    let report = Report { settings_file: state, handed_to_engine: paths.len(), issues };
    (paths, report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(label: &str) -> tempfile::TempDir {
        let _ = label;
        tempfile::tempdir().unwrap()
    }

    fn db(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"x").unwrap();
        p
    }

    fn settings(dir: &Path, bytes: &[u8]) -> PathBuf {
        let file = dir.join("settings.json");
        std::fs::write(&file, bytes).unwrap();
        file
    }

    #[test]
    fn no_file_and_no_env_is_the_default() {
        let dir = temp("none");
        let (paths, report) = resolve(&dir.path().join("absent.json"), None, None);
        assert!(paths.is_empty() && report.issues.is_empty());
        assert_eq!(report.settings_file, "absent");
    }

    #[test]
    fn file_and_env_naming_the_same_db_count_once() {
        let dir = temp("same");
        let a = db(dir.path(), "opencode.db");
        let json = serde_json::json!({"scanner": {"opencodeDbPaths": [a, a]}});
        let file = settings(dir.path(), json.to_string().as_bytes());
        let (paths, report) = resolve(&file, Some(a.clone().into_os_string()), None);
        assert_eq!(paths, vec![a]);
        assert!(report.issues.is_empty());
    }

    #[test]
    fn bom_quotes_and_tilde() {
        let dir = temp("norm");
        let home = dir.path();
        db(home, "opencode-beta.db");
        let json = serde_json::json!({"scanner": {"opencodeDbPaths": ["~/opencode-beta.db"]}});
        let mut bytes = b"\xEF\xBB\xBF".to_vec();
        bytes.extend(json.to_string().as_bytes());
        let file = settings(home, &bytes);
        let b = db(home, "opencode.db");
        let quoted = format!("  \"{}\" ", b.display());
        let (paths, report) = resolve(&file, Some(quoted.into()), Some(home));
        assert_eq!(report.settings_file, "ok");
        assert_eq!(paths, vec![home.join("opencode-beta.db"), b]);
        assert!(report.issues.is_empty());
        assert_eq!(normalize("~", Some(home)), home);
        // `~user`, `$HOME` and a home-less `~` are not expanded.
        assert_eq!(normalize("~x/a.db", Some(home)), PathBuf::from("~x/a.db"));
        assert_eq!(normalize("$HOME/a.db", Some(home)), PathBuf::from("$HOME/a.db"));
        assert_eq!(normalize("~/a.db", None), PathBuf::from("~/a.db"));
    }

    #[test]
    fn rejections_carry_a_reason_and_never_a_path() {
        let dir = temp("rej");
        let d = dir.path();
        let ok = db(d, "opencode.db");
        let missing = d.join("opencode-gone.db");
        let json = serde_json::json!({"scanner": {"opencodeDbPaths": [
            "rel/opencode.db", "~x/opencode.db", d.join("mydb.db"), missing, ok]}});
        let file = settings(d, json.to_string().as_bytes());
        let (paths, report) = resolve(&file, None, Some(d));
        // Only the non-absolute ones are withheld; the engine decides on the rest.
        assert_eq!(paths, vec![d.join("mydb.db"), missing, ok]);
        assert_eq!(report.handed_to_engine, 3);
        let got: Vec<_> = report.issues.iter().map(|r| (r.index, r.reason)).collect();
        assert_eq!(
            got,
            vec![
                (0, "notAbsolute"),
                (1, "notAbsolute"),
                (2, "badFileName"),
                (2, "missing"),
                (3, "missing")
            ]
        );
    }

    /// A path that is not valid Unicode reaches the engine byte-for-byte.
    #[test]
    fn a_non_unicode_env_path_is_passed_through_untouched() {
        use std::os::unix::ffi::OsStringExt;
        let dir = temp("nonuni");
        let mut bytes = dir.path().as_os_str().as_encoded_bytes().to_vec();
        bytes.extend(b"/bad\xFF/opencode.db");
        let raw = std::ffi::OsString::from_vec(bytes);
        let (paths, report) = resolve(&dir.path().join("absent.json"), Some(raw.clone()), None);
        assert_eq!(paths, vec![PathBuf::from(&raw)]);
        assert_eq!(report.handed_to_engine, 1);
        assert!(report.issues.iter().any(|i| i.reason == "missing"));
    }

    #[test]
    fn unusable_files_are_reported_and_ignored() {
        let dir = temp("junk");
        let d = dir.path();
        let file = settings(d, b"{not json");
        assert_eq!(resolve(&file, None, None).1.settings_file, "invalidJson");
        let file = settings(d, &vec![b' '; MAX_SETTINGS_BYTES as usize + 1]);
        let (paths, report) = resolve(&file, None, None);
        assert!(paths.is_empty());
        assert_eq!(report.settings_file, "tooLarge");
        let file = settings(d, br#"{"scanner":{"extraScanPaths":{"claude":["/x"]}}}"#);
        let (paths, report) = resolve(&file, None, None);
        assert!(paths.is_empty() && report.settings_file == "ok");
    }
}
