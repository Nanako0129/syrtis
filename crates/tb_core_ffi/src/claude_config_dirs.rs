//! Process-wide registry of the extra Claude config directories the user has
//! configured (`CLAUDE_CONFIG_DIR`-isolated accounts). Written by
//! `tb_set_claude_config_dirs`, read by the Claude quota fetch.
//!
//! **Not the same value as `extra_scan_paths`, on purpose.** That registry
//! holds the expanded `<dir>/projects` and `<dir>/transcripts` sub-roots and
//! answers "which directories does the scanner walk". This one holds the
//! config directories themselves and answers "whose credential is this quota
//! card fetched with" — the directory selects the Keychain item
//! (`claude_keychain_service`) and therefore the durable history identity
//! (`claude_history_scope`). The two share one user input and nothing else:
//! a directory the scanner rejects is still a perfectly valid account, and
//! the registered scan subset is empty until the launch-time apply lands,
//! which is an honest answer for scanning and a wrong one for identity.
//!
//! A `RwLock` static rather than an env var, for the same reason
//! `extra_scan_paths` is one: the process is resident and a Settings edit
//! must take effect without a restart.

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, RwLock};

/// At most this many extra config directories are registered; the rest are
/// refused with `limit_reached`. Each is its own quota fetch on every poll, and
/// Syrtis-Windows uses the same cap.
pub(crate) const MAX_CONFIG_DIRS: usize = 8;

/// Why a directory was refused. `code()` is the whole wire text: a refusal is
/// reported by its index in the input and this code, never with the path or an
/// OS error, so Swift maps it to a fixed sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    EmptyPath,
    NotAbsolute,
    FilesystemRoot,
    /// No home directory is known, so the primary's roots cannot be checked.
    /// Fails closed rather than accepting a directory that might be them.
    HomeUnknown,
    HomeDirectory,
    /// `PrimaryRoots::collides`: `~/.claude` or an ancestor of it, or a primary
    /// scan root, its ancestor or its descendant.
    PrimaryScanRoot,
    Duplicate,
    /// Contains or sits inside a directory accepted earlier in the list.
    Overlap,
    LimitReached,
}

impl Refusal {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::EmptyPath => "empty_path",
            Self::NotAbsolute => "not_absolute",
            Self::FilesystemRoot => "filesystem_root",
            Self::HomeUnknown => "home_unknown",
            Self::HomeDirectory => "home_directory",
            Self::PrimaryScanRoot => "primary_scan_root",
            Self::Duplicate => "duplicate",
            Self::Overlap => "overlap",
            Self::LimitReached => "limit_reached",
        }
    }
}

/// Every directory the primary Claude account's scan reads under `home`: its
/// two roots in `~/.claude`, the Cowork sessions tree (under which the engine
/// discovers `<session>/.claude/projects` roots) and the `.cc-mirror` variants'
/// project roots that sit under `~/.cc-mirror`.
///
/// The cc-mirror roots are the engine's own answer
/// (`built_in_extra_scan_paths_for`, the public surface of
/// `cc_mirror::discover_claude_project_roots`), so a variant is treated as the
/// engine treats it: a `variant.json` that is missing or unreadable means the
/// variant is not scanned and contributes nothing; no `configDir` means
/// `<variant>/config`. Only the roots under `~/.cc-mirror` are taken: their
/// rows are tagged `cc-mirror/<variant>` by their path, so an extra account
/// there would show on no card of its own and drop out of the primary's window.
/// A variant whose `configDir` names a folder elsewhere is not a primary scan
/// root: that folder is an ordinary Claude config dir and may be an account
/// (issue #258).
///
/// This reads the disk: a `read_dir` of `~/.cc-mirror`, each variant's
/// `variant.json`, and the engine's walk of the Cowork tree. Nothing leaves the
/// machine. Compute it once per call (`PrimaryRoots::for_home`), never per
/// entry, and not on the main thread.
fn primary_scan_roots(home: &Path) -> Vec<PathBuf> {
    let claude = home.join(".claude");
    let mut roots = vec![
        claude.join("projects"),
        claude.join("transcripts"),
        home.join("Library/Application Support/Claude/local-agent-mode-sessions"),
    ];
    let mirror = home.join(".cc-mirror");
    let enabled = std::collections::HashSet::from([tokscale_core::ClientId::Claude]);
    roots.extend(
        tokscale_core::scanner::built_in_extra_scan_paths_for(&home.to_string_lossy(), &enabled)
            .into_iter()
            .map(|(_, path)| path)
            .filter(|path| path.starts_with(&mirror)),
    );
    roots
}

// Reads of the disk by `PrimaryRoots::for_home` on this thread, so a test can
// show a call reads once, not once per entry.
#[cfg(test)]
thread_local! {
    static FOR_HOME_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The primary's scan roots under one home, read once. `collides` is the one
/// rule behind three consumers: the config-dir setter, the Swift pre-check
/// (through `tb_claude_config_dir_refusals`) and
/// `window_usage::primary_exclusions`. True for `~/.claude` itself and every
/// ancestor of it, and for each primary scan root, its ancestors and its
/// descendants. An extra account there would make the primary window exclude
/// its own transcripts (the P1 of the 2026-11-03 multi-account audit).
///
/// A folder inside `~/.claude` but outside its scan roots, such as
/// `~/.claude/accounts/work`, is allowed: the primary scans only those roots,
/// so such a folder does not reduce the primary's usage, and refusing it would
/// drop accounts users already keep there.
pub(crate) struct PrimaryRoots {
    claude: PathBuf,
    roots: Vec<PathBuf>,
}

impl PrimaryRoots {
    pub(crate) fn for_home(home: &Path) -> Self {
        #[cfg(test)]
        FOR_HOME_READS.with(|reads| reads.set(reads.get() + 1));
        Self {
            claude: folded(&home.join(".claude")),
            roots: primary_scan_roots(home),
        }
    }

    pub(crate) fn collides(&self, path: &Path) -> bool {
        self.claude.starts_with(folded(path)) || self.roots.iter().any(|root| overlaps(path, root))
    }
}

/// Lower-cased for comparison. The default APFS volume is case-insensitive, so
/// `~/.CLAUDE` is `~/.claude`. Symlink and Unicode-normalisation aliases are
/// not resolved (MA-9 symlink identity is deferred): comparing spelling avoids
/// a `stat` on a path that may sit on a stalled mount.
fn folded(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

/// `a` equals, contains or sits inside `b`, component-wise and ignoring case.
pub(crate) fn overlaps(a: &Path, b: &Path) -> bool {
    let (a, b) = (folded(a), folded(b));
    a.starts_with(&b) || b.starts_with(&a)
}

/// The rule shared by the setter and the Swift pre-check: which entries of
/// `input` are registered, in order, and which are refused, by index. It reads
/// the disk once (`PrimaryRoots::for_home`), not once per entry.
/// Earlier entries win: a later duplicate or overlapping entry is refused, and
/// refused entries do not count toward the cap or against later entries.
pub(crate) fn check(input: &[String], home: Option<&Path>) -> (Vec<String>, Vec<(usize, Refusal)>) {
    let mut accepted: Vec<String> = Vec::new();
    let mut refused: Vec<(usize, Refusal)> = Vec::new();
    // Read the disk only when there is something to judge.
    let primary = home
        .filter(|_| !input.is_empty())
        .map(PrimaryRoots::for_home);
    for (index, raw) in input.iter().enumerate() {
        match refusal(raw, home.zip(primary.as_ref()), &accepted) {
            Ok(dir) => accepted.push(dir),
            Err(reason) => refused.push((index, reason)),
        }
    }
    (accepted, refused)
}

fn refusal(
    raw: &str,
    primary: Option<(&Path, &PrimaryRoots)>,
    accepted: &[String],
) -> Result<String, Refusal> {
    let dir = normalize(raw)?;
    let path = Path::new(&dir);
    let (home, primary) = primary.ok_or(Refusal::HomeUnknown)?;
    if folded(path) == folded(home) {
        return Err(Refusal::HomeDirectory);
    }
    if primary.collides(path) {
        return Err(Refusal::PrimaryScanRoot);
    }
    if accepted
        .iter()
        .any(|other| folded(Path::new(other)) == folded(path))
    {
        return Err(Refusal::Duplicate);
    }
    if accepted
        .iter()
        .any(|other| overlaps(path, Path::new(other)))
    {
        return Err(Refusal::Overlap);
    }
    if accepted.len() >= MAX_CONFIG_DIRS {
        return Err(Refusal::LimitReached);
    }
    Ok(dir)
}

/// `check` for the FFI pre-check: `{"rejected":[{"index","reason"}]}` for a
/// JSON array of paths, judged against the process home. Registers nothing,
/// but reads the disk (see `primary_scan_roots`).
pub(crate) fn refusals_from_json(raw: &str) -> Result<serde_json::Value, String> {
    let input: Vec<String> =
        serde_json::from_str(raw).map_err(|_| "invalid_config_dirs_json".to_string())?;
    let (_, refused) = check(&input, crate::user_home_dir().as_deref());
    Ok(serde_json::json!({ "rejected": rejected_json(&refused) }))
}

fn rejected_json(refused: &[(usize, Refusal)]) -> Vec<serde_json::Value> {
    refused
        .iter()
        .map(|(index, reason)| serde_json::json!({ "index": index, "reason": reason.code() }))
        .collect()
}

static CLAUDE_CONFIG_DIRS: LazyLock<RwLock<Vec<String>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Configured extra config directories, in the order the user listed them.
/// Empty by default — a process that never calls the setter fetches exactly
/// one Claude account, as before this feature existed.
pub(crate) fn snapshot() -> Vec<String> {
    CLAUDE_CONFIG_DIRS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Trailing separators change the SHA-256 the Keychain service name is derived
/// from, so `/x/.claude-work/` would read no item at all while looking
/// identical in Settings. A relative path is refused rather than resolved: it
/// would resolve against whatever the process CWD happens to be, and the
/// resulting identity would move with it.
///
/// Whitespace is deliberately NOT trimmed, though an earlier version did. The
/// Swift side sends `standardizingPath`'s output verbatim and that call does
/// not strip surrounding whitespace, so trimming here would derive the Keychain
/// service from a string that is not the directory: a folder whose name really
/// ends in a space is reachable through the picker, its transcripts would be
/// scanned under the real path, and its quota card would report no login found
/// forever. Whitespace is a legal character in a path, and the two derivations
/// of one identity have to agree before either of them has to look tidy.
fn normalize(raw: &str) -> Result<String, Refusal> {
    if raw.is_empty() {
        return Err(Refusal::EmptyPath);
    }
    if !Path::new(raw).is_absolute() {
        return Err(Refusal::NotAbsolute);
    }
    let stripped = raw.trim_end_matches('/');
    if stripped.is_empty() {
        return Err(Refusal::FilesystemRoot);
    }
    Ok(stripped.to_string())
}

/// Replace the whole registry from a JSON array of absolute directory paths.
/// Full-replace, not merge: `[]` clears every configured account (the Settings
/// rollback path). Refusals follow `check`, against the process home.
///
/// Existence is deliberately not probed. Whether a directory can be read right
/// now says nothing about which account its Keychain item belongs to, and the
/// answer would go stale the moment a volume mounts. It also keeps this setter
/// off the `stat` path that made `extra_scan_paths` block its caller.
pub(crate) fn set_from_json(raw: &str) -> Result<serde_json::Value, String> {
    set_from_json_with_home(raw, crate::user_home_dir().as_deref())
}

pub(crate) fn set_from_json_with_home(
    raw: &str,
    home: Option<&Path>,
) -> Result<serde_json::Value, String> {
    let input: Vec<String> =
        serde_json::from_str(raw).map_err(|e| format!("invalid Claude config dirs JSON: {}", e))?;
    let (registered, refused) = check(&input, home);

    let registered_count = registered.len();
    *CLAUDE_CONFIG_DIRS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = registered;

    Ok(serde_json::json!({
        "registeredCount": registered_count,
        "rejected": rejected_json(&refused),
    }))
}

/// One process-wide mutex for every test that reads or writes the static, so
/// parallel `cargo test` threads do not observe each other's registry. Public
/// to the crate because `agent_usage` and `lib` both drive it.
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn reset_for_test() {
    *CLAUDE_CONFIG_DIRS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Vec::new();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_absolute_dirs_and_reports_what_it_refused() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_test();

        let result = set_from_json_with_home(
            r#"["/Users/x/.claude-work/", "/Users/x/claude dir ", "relative/dir", "", "/Users/x/.claude-work"]"#,
            Some(Path::new("/Users/x")),
        )
        .unwrap();

        assert_eq!(result["registeredCount"], 2);
        assert_eq!(
            snapshot(),
            vec![
                "/Users/x/.claude-work".to_string(),
                "/Users/x/claude dir ".to_string()
            ],
            "a trailing separator would derive a different Keychain service, and \
             trimming the trailing SPACE would derive one for a directory that is \
             not the one Swift registered for scanning — that account's \
             transcripts would be read while its quota card reported no login"
        );
        assert_eq!(
            result["rejected"],
            serde_json::json!([
                { "index": 2, "reason": "not_absolute" },
                { "index": 3, "reason": "empty_path" },
                { "index": 4, "reason": "duplicate" },
            ]),
            "refusals are reported by index and fixed code, never with the path"
        );
        // And a path that is nothing but whitespace is still refused, by the
        // absolute-path rule rather than by trimming it into emptiness.
        assert!(set_from_json(r#"["   "]"#).unwrap()["registeredCount"] == 0);

        // Full-replace, including back to nothing.
        set_from_json("[]").unwrap();
        assert!(snapshot().is_empty());
        reset_for_test();
    }

    fn codes(input: &[&str], home: Option<&Path>) -> Vec<(usize, &'static str)> {
        let input: Vec<String> = input.iter().map(|s| s.to_string()).collect();
        check(&input, home)
            .1
            .into_iter()
            .map(|(index, reason)| (index, reason.code()))
            .collect()
    }

    /// The primary's roots are read from the disk once per call, not once per
    /// entry: `check` over several entries reads once, over none reads not at
    /// all. Mutation: build the roots inside the loop.
    #[test]
    fn the_primary_roots_are_read_once_per_check() {
        let tmp = tempfile::tempdir().unwrap();
        let reads = || FOR_HOME_READS.with(std::cell::Cell::get);
        let before = reads();
        let many: Vec<String> = (0..5).map(|i| format!("/x/account-{i}")).collect();
        check(&many, Some(tmp.path()));
        assert_eq!(reads() - before, 1, "one read for five entries");
        let before = reads();
        check(&[], Some(tmp.path()));
        assert_eq!(reads() - before, 0, "nothing to judge, nothing read");
    }

    /// The cc-mirror roots are the engine's own discovery, so each variant is
    /// treated as the engine treats it: a default-layout variant
    /// (`<variant>/config/projects` with a readable `variant.json`, no
    /// `configDir`) is a primary scan root and is refused with its ancestors
    /// and descendants; a variant with a `configDir` override is scanned at
    /// that folder, which is not refused; a variant with no `variant.json` or
    /// an unreadable one is not scanned at all, so nothing there is refused.
    #[test]
    fn cc_mirror_roots_follow_the_engines_own_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let write_variant = |name: &str, json: Option<&str>| {
            let variant = home.join(".cc-mirror").join(name);
            std::fs::create_dir_all(variant.join("config/projects")).unwrap();
            if let Some(json) = json {
                std::fs::write(variant.join("variant.json"), json).unwrap();
            }
            variant.join("config")
        };
        let default_layout = write_variant("default", Some("{}"));
        let no_json = write_variant("no-json", None);
        let bad_json = write_variant("bad-json", Some("not json"));
        let overridden = home.join("elsewhere");
        std::fs::create_dir_all(overridden.join("projects")).unwrap();
        let override_variant = write_variant(
            "override",
            Some(&serde_json::json!({ "configDir": overridden }).to_string()),
        );
        let roots = PrimaryRoots::for_home(home);
        let collides = |p: &Path| roots.collides(p);
        // Refused: the default layout's folder, its projects root, an ancestor
        // and a descendant.
        for refused in [
            default_layout.clone(),
            default_layout.join("projects"),
            default_layout.join("projects/p"),
            default_layout.parent().unwrap().to_path_buf(),
        ] {
            assert!(collides(&refused), "{refused:?} was accepted");
        }
        // Accepted: an override's folder (the engine scans it, but it is an
        // ordinary config dir), the override variant's own default folder (the
        // engine ignores it once `configDir` is set), and variants the engine
        // does not scan.
        for accepted in [&overridden, &override_variant, &no_json, &bad_json] {
            assert!(!collides(accepted), "{accepted:?} was refused");
        }
        // Through the rule the setter and the Swift pre-check share.
        let list = |dirs: &[&Path]| {
            dirs.iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
        };
        let (accepted, refused) = check(
            &list(&[&default_layout, &overridden, &no_json, &bad_json]),
            Some(home),
        );
        assert_eq!(
            refused
                .iter()
                .map(|(i, r)| (*i, r.code()))
                .collect::<Vec<_>>(),
            vec![(0, "primary_scan_root")]
        );
        assert_eq!(accepted.len(), 3);
    }

    /// Every primary scan root, its ancestors and its descendants are refused,
    /// compared without case; an unrelated folder beside them is accepted.
    #[test]
    fn refuses_every_primary_scan_root_its_ancestors_and_descendants() {
        let home = Some(Path::new("/Users/x"));
        for dir in [
            "/",
            "/Users",
            "/Users/x/.claude",
            "/Users/x/.claude/",
            "/Users/x/.CLAUDE",
            "/Users/x/.claude/projects",
            "/Users/x/.claude/projects/p",
            "/Users/x/.claude/transcripts",
            "/Users/x/.claude/transcripts/t",
            "/Users/x/Library",
            "/Users/x/Library/Application Support/Claude",
            "/Users/x/library/application support/claude",
            "/Users/x/Library/Application Support/Claude/local-agent-mode-sessions",
            "/Users/x/Library/Application Support/Claude/local-agent-mode-sessions/w/s",
        ] {
            let got = codes(&[dir], home);
            assert!(
                matches!(
                    got.as_slice(),
                    [(0, "primary_scan_root" | "filesystem_root")]
                ),
                "{dir} must be refused as a primary scan root, got {got:?}"
            );
        }
        assert_eq!(codes(&["/Users/x"], home), vec![(0, "home_directory")]);
        assert_eq!(codes(&["/Users/X/"], home), vec![(0, "home_directory")]);
        // Inside `~/.claude` but outside its scan roots: allowed (the primary
        // scans only those roots), unlike `~/.claude/projects/p` above.
        assert!(codes(
            &["/Users/x/.claude/accounts/work", "/Users/x/.claude/w"],
            home
        )
        .is_empty());
        // Control: siblings that share a prefix but not a component.
        assert!(codes(
            &[
                "/Users/x/.claude-work",
                "/Users/x/Library/Application Support/Claude-other"
            ],
            home
        )
        .is_empty());
        // No home: nothing can be checked against the primary, so fail closed.
        assert_eq!(
            codes(&["/Users/x/.claude-work"], None),
            vec![(0, "home_unknown")]
        );
    }

    #[test]
    fn refuses_overlap_case_insensitive_duplicates_and_more_than_eight() {
        let home = Some(Path::new("/Users/x"));
        assert_eq!(
            codes(&["/w/a", "/w/a/sub", "/w", "/W/A", "/w/b"], home),
            vec![(1, "overlap"), (2, "overlap"), (3, "duplicate")],
            "the first entry wins; a later nested, enclosing or re-cased one is refused"
        );
        let nine: Vec<String> = (0..9).map(|i| format!("/w/{i}")).collect();
        let nine: Vec<&str> = nine.iter().map(String::as_str).collect();
        assert_eq!(codes(&nine, home), vec![(8, "limit_reached")]);
        // A refused entry does not use up a slot or block a later one.
        let mut with_refused = vec!["/Users/x/.claude"];
        with_refused.extend(&nine[..8]);
        assert_eq!(codes(&with_refused, home), vec![(0, "primary_scan_root")]);
    }

    #[test]
    fn malformed_json_leaves_the_registry_untouched() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_test();
        set_from_json(r#"["/Users/x/.claude-work"]"#).unwrap();
        let error = set_from_json("{not json").unwrap_err();
        assert!(error.contains("invalid Claude config dirs JSON"), "{error}");
        assert_eq!(snapshot(), vec!["/Users/x/.claude-work".to_string()]);
        reset_for_test();
    }
}
