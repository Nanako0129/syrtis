//! Benchmarks for the live tail and the graph recompute on a captured corpus.
//! Run one bench per invocation (paths absolute — `cargo test` runs the binary
//! from the package directory):
//!
//! ```text
//! TOKSCALE_CONFIG_DIR=<corpus>/cfg TOKSCALE_PRICING_CACHE_ONLY=1 \
//!   BENCH_CORPUS=<corpus> BENCH_NOW_MS=<capture time in ms> \
//!   cargo test -p tb_core_ffi --release --lib usage_tail::bench::bench_tail_tick \
//!   -- --exact --ignored --nocapture
//! ```
//!
//! The corpus is a directory holding `IDENTITY`, `home/` (copied session data,
//! mtimes preserved — the tail prunes by mtime — and not hard-linked to the
//! originals) and `cfg/` (the message cache, plus a pricing cache at
//! `cfg/cache/pricing-litellm.json`). The bench reads `home/` through an
//! injected context that ignores per-client root variables (CODEX_HOME and the
//! like), so the process HOME stays real for the toolchain, and pins rayon's
//! pool to the app's two threads. It refuses a corpus that is malformed or
//! already used: a run writes the message cache and, in append mode, a session
//! file, so each run needs a fresh copy (it claims the corpus with a
//! `BENCH_USED` marker once every setting has been checked).
//!
//!   BENCH_CORPUS       the corpus directory (required)
//!   BENCH_NOW_MS       fixed clock in Unix milliseconds (required), normally
//!                      the capture time
//!   BENCH_TICKS        ticks after the first, 1..3599 (default 20)
//!   BENCH_GRAPH_ITERS  graph recomputes, 1..3599 (default 10)
//!   BENCH_APPEND       file under `home/` (relative) to append one synthesized
//!                      line to per tick; required by the graph bench
//!   BENCH_APPEND_KIND  `claude` (default) or `codex`
//!
//! A synthesized line carries a fresh identity (Claude ids; Codex cumulative
//! totals), because re-appending an existing line is dropped by dedup and would
//! never add an event. Its timestamp is not always the one written: a Claude
//! assistant line right after a user or tool_result line takes that line's
//! request start, and a Codex token_count takes the previous accepted
//! token_count's time — so the target's last real record should be inside the
//! window. A degenerate run fails rather than reporting timings: an empty tick0
//! window, an append that adds no event to its lane, unchanged mode that parsed
//! more than once, a graph append that did not move the token, or a failed
//! probe or recompute.
//!
//! Limits — where the numbers differ from the app:
//! - The digest covers the tail's event window, not `rate_in_window` or
//!   `trace` (they still read the wall clock).
//! - The graph bench measures cost only (no payload digest). It runs the token
//!   probe and `usage_graph::run` as `graph_compute` does, but not the payload
//!   clone `publish_graph` keeps or the FFI serialization, and it times one
//!   probe per recompute where the app's `tb_graph` makes two.
//! - With TOKSCALE_PRICING_CACHE_ONLY=1 every recompute re-reads the pricing
//!   cache from disk; the app keeps prices in memory for an hour, so
//!   `run_cpu_ms` overstates that part.
//! - Claude extra roots registered from the app's Settings are not registered
//!   here, and root paths stored inside corpus files (a cc-mirror variant's
//!   configDir, Crush's registry) are followed as written, even outside it.
//! - CPU comes from a hand-declared getrusage with Darwin's layout, hence
//!   macOS only.
use super::{UsageEvent, UsageTailer};
use crate::LocalSourceContext;
use chrono::{SecondsFormat, TimeZone, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

/// Both benches read whole-process CPU and may append to the same file.
static BENCH_LOCK: Mutex<()> = Mutex::new(());

#[repr(C)]
#[derive(Default)]
struct Timeval {
    sec: i64,
    usec: i32, // Darwin's suseconds_t.
}
#[repr(C)]
#[derive(Default)]
struct Rusage {
    utime: Timeval,
    stime: Timeval,
    rest: [i64; 14],
}
extern "C" {
    fn getrusage(who: i32, usage: *mut Rusage) -> i32;
}

/// Process user+sys CPU in ms, every thread included.
fn cpu_ms() -> f64 {
    let mut r = Rusage::default();
    assert_eq!(unsafe { getrusage(0, &mut r) }, 0);
    let ms = |t: &Timeval| t.sec as f64 * 1000.0 + t.usec as f64 / 1000.0;
    ms(&r.utime) + ms(&r.stime)
}

/// Every field, totally ordered, then hashed: two windows match only when they
/// hold the same multiset of events.
fn digest(events: &[UsageEvent]) -> String {
    let mut rows: Vec<_> = events
        .iter()
        .map(|e| {
            (
                e.ts_ms,
                e.client.as_str(),
                e.agent.as_str(),
                e.model.as_str(),
                (e.input, e.output, e.reasoning, e.cache_read, e.cache_write),
                e.message_count,
            )
        })
        .collect();
    rows.sort();
    let hash = Sha256::digest(serde_json::to_vec(&rows).expect("serialize rows"));
    hash.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn last_line_with(path: &Path, needles: &[&str], keep: impl Fn(&Value) -> bool) -> Value {
    std::io::BufReader::new(std::fs::File::open(path).expect("open append target"))
        .split(b'\n')
        // Stop on an I/O error; skip a line that is not UTF-8, as the parsers do.
        .map_while(Result::ok)
        .filter_map(|bytes| String::from_utf8(bytes).ok())
        .filter(|l| needles.iter().all(|n| l.contains(n)))
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .filter(|v| keep(v))
        .last()
        .unwrap_or_else(|| panic!("no matching line with {needles:?} in {}", path.display()))
}

/// A Claude line the parser turns into an event: an assistant turn with usage
/// and a real model (the parser skips model-less and `<synthetic>` placeholder
/// turns, so a copy of one would never add an event).
fn is_claude_source(v: &Value) -> bool {
    v["type"] == "assistant"
        && v["message"]["usage"].is_object()
        && v["message"]["model"].as_str().is_some_and(|m| m != "<synthetic>")
}

/// Appends `line` as its own record, adding the separator first when the
/// file's last record has no trailing newline (otherwise the two would join
/// into one invalid line).
fn append_record(path: &Path, line: &str) {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .open(path)
        .expect("open append");
    let mut last = [0u8; 1];
    let unterminated = f.metadata().expect("stat append").len() > 0
        && f.seek(SeekFrom::End(-1)).is_ok()
        && f.read_exact(&mut last).is_ok()
        && last[0] != b'\n';
    if unterminated {
        writeln!(f).expect("append separator");
    }
    writeln!(f, "{line}").expect("append");
}

/// The `n`-th synthesized line from `source` (the target's latest event-bearing
/// line, read once): a fresh identity and `ts_ms`; for Codex, totals `n`
/// steps above the source's, so each line is strictly newer than the last.
fn synthesize(source: &Value, kind: &str, n: i64, ts_ms: i64) -> String {
    let mut v = source.clone();
    v["timestamp"] = Utc
        .timestamp_millis_opt(ts_ms)
        .single()
        .expect("timestamp")
        .to_rfc3339_opts(SecondsFormat::Millis, true)
        .into();
    if kind == "claude" {
        v["uuid"] = format!("bench-uuid-{n}").into();
        v["requestId"] = format!("req_bench_{n}").into();
        v["message"]["id"] = format!("msg_bench_{n}").into();
    } else {
        let delta = [("input_tokens", 1000), ("output_tokens", 100), ("total_tokens", 1100)];
        let info = &mut v["payload"]["info"];
        for (key, add) in delta {
            let total = info["total_token_usage"][key].as_i64().unwrap_or(0);
            info["total_token_usage"][key] = (total + add * n).into();
        }
        // Zero every cached field: the parser takes the larger of the two names.
        for key in [
            "cached_input_tokens",
            "cache_read_input_tokens",
            "cache_write_input_tokens",
            "reasoning_output_tokens",
        ] {
            info["last_token_usage"][key] = 0.into();
        }
        for (key, add) in delta {
            info["last_token_usage"][key] = add.into();
        }
    }
    serde_json::to_string(&v).expect("serialize line")
}

fn lanes(events: &[UsageEvent]) -> String {
    let mut by: BTreeMap<&str, (usize, i64)> = BTreeMap::new();
    for e in events {
        let slot = by.entry(e.client.as_str()).or_default();
        slot.0 += 1;
        slot.1 = slot.1.saturating_add(e.total());
    }
    by.iter()
        .map(|(client, (n, tokens))| format!("{client}={n}/{tokens}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Accepts `corpus` only when it is a fresh, well-formed corpus the cache and
/// pricing settings point into; returns its canonical root and `home/`. Pure
/// checks, so the guard is unit-tested rather than living only in the benches.
fn check_corpus(
    corpus: &Path,
    config_dir: Option<&str>,
    pricing_cache_only: Option<&str>,
) -> Result<(PathBuf, PathBuf), String> {
    let corpus = corpus.canonicalize().map_err(|e| format!("corpus {}: {e}", corpus.display()))?;
    if !corpus.join("IDENTITY").is_file() {
        return Err(format!("{} has no IDENTITY: not a corpus", corpus.display()));
    }
    if corpus.join("BENCH_USED").exists() {
        return Err(format!("{} was already used by a bench run; make a fresh copy", corpus.display()));
    }
    let home = corpus.join("home");
    if !home.is_dir() {
        return Err(format!("{} has no home/", corpus.display()));
    }
    let cfg = config_dir.ok_or("TOKSCALE_CONFIG_DIR must point into the corpus")?;
    let cfg = Path::new(cfg).canonicalize().map_err(|e| format!("TOKSCALE_CONFIG_DIR {cfg}: {e}"))?;
    // The cache subdirectory is where the run writes; a link there would send
    // those writes outside the corpus even though `cfg` itself is inside.
    let cache = cfg.join("cache").canonicalize().map_err(|e| format!("{}/cache: {e}", cfg.display()))?;
    if !cfg.starts_with(&corpus) || !cache.starts_with(&corpus) {
        return Err(format!("TOKSCALE_CONFIG_DIR {} or its cache is outside the corpus", cfg.display()));
    }
    if pricing_cache_only != Some("1") {
        return Err("TOKSCALE_PRICING_CACHE_ONLY must be 1, or a recompute may fetch prices".into());
    }
    // Cache-only pricing with no cache prices nothing, which is cheaper than
    // the app and would still pass.
    if !cache.join("pricing-litellm.json").is_file() {
        return Err(format!("{} has no pricing-litellm.json", cache.display()));
    }
    Ok((corpus, home))
}

/// Resolves the append target: a single-link regular file under `home`. A
/// hard link would write through to the original session log.
fn check_append(home: &Path, rel: &str) -> Result<PathBuf, String> {
    let file = home
        .join(rel)
        .canonicalize()
        .map_err(|e| format!("BENCH_APPEND {rel}: {e}"))?;
    let meta = std::fs::metadata(&file).map_err(|e| format!("BENCH_APPEND {rel}: {e}"))?;
    if !meta.is_file() || !file.starts_with(home) {
        return Err(format!("BENCH_APPEND {rel} is not a file under home/"));
    }
    if meta.nlink() > 1 {
        return Err(format!("BENCH_APPEND {rel} has {} hard links; copy the corpus instead", meta.nlink()));
    }
    Ok(file)
}

/// Parses a count setting within `range`; a malformed or out-of-range value
/// fails the run instead of falling back to the default.
fn count(name: &str, value: Option<&str>, default: usize, range: std::ops::Range<usize>) -> Result<usize, String> {
    let n = match value {
        None => default,
        Some(v) => v.parse().map_err(|_| format!("{name} {v:?} is not an integer"))?,
    };
    if range.contains(&n) {
        Ok(n)
    } else {
        Err(format!("{name} {n} is outside {range:?}"))
    }
}

/// A setting from the environment; set-but-not-UTF-8 fails rather than
/// counting as unset.
fn setting(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) => Some(v),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => panic!("{name} is not valid UTF-8"),
    }
}

/// Validated settings for one bench run.
struct BenchEnv {
    corpus: PathBuf,
    context: LocalSourceContext,
    home: PathBuf,
    now: i64,
    append: Option<(PathBuf, Value)>,
    kind: String,
}

impl BenchEnv {
    /// Checks every setting; claims nothing (see `claim`).
    fn read() -> Self {
        let corpus = setting("BENCH_CORPUS").expect("BENCH_CORPUS");
        let (corpus, home) = check_corpus(
            Path::new(&corpus),
            setting("TOKSCALE_CONFIG_DIR").as_deref(),
            setting("TOKSCALE_PRICING_CACHE_ONLY").as_deref(),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let now_raw = setting("BENCH_NOW_MS").expect("BENCH_NOW_MS (a fixed clock is required)");
        let now: i64 = now_raw
            .parse()
            .unwrap_or_else(|_| panic!("BENCH_NOW_MS {now_raw:?} is not an integer"));
        // 2001..2286 in milliseconds: rejects seconds, micro- and nanoseconds.
        assert!(
            (1_000_000_000_000..10_000_000_000_000).contains(&now),
            "BENCH_NOW_MS {now} is not a millisecond timestamp"
        );
        let kind = setting("BENCH_APPEND_KIND").unwrap_or_else(|| "claude".into());
        assert!(matches!(kind.as_str(), "claude" | "codex"), "BENCH_APPEND_KIND {kind}");
        let append = setting("BENCH_APPEND").map(|rel| {
            let file = check_append(&home, &rel).unwrap_or_else(|e| panic!("{e}"));
            let source = match kind.as_str() {
                "claude" => last_line_with(&file, &["\"usage\""], is_claude_source),
                _ => last_line_with(&file, &["\"token_count\"", "\"total_token_usage\""], |_| true),
            };
            (file, source)
        });
        Self {
            corpus,
            context: LocalSourceContext::for_corpus(home.clone()),
            home,
            now,
            append,
            kind,
        }
    }

    /// Marks the corpus used, atomically, once every setting has passed: a run
    /// that fails validation leaves the corpus reusable, and two runs started
    /// at once cannot both claim it.
    fn claim(&self) {
        let marker = self.corpus.join("BENCH_USED");
        if let Err(e) = std::fs::OpenOptions::new().write(true).create_new(true).open(&marker) {
            panic!("cannot claim {}: {e} (already used? make a fresh copy)", self.corpus.display());
        }
        println!(
            "clock\tnow_ms={}\trayon_threads={}\tcorpus={}",
            self.now,
            rayon::current_num_threads(),
            self.home.display()
        );
    }

    /// Appends the `n`-th synthesized line, stamped `back_secs` before the clock.
    fn append(&self, n: usize, back_secs: i64) {
        if let Some((file, source)) = &self.append {
            let line = synthesize(source, &self.kind, n as i64, self.now - back_secs * 1000);
            append_record(file, &line);
        }
    }
}

fn report(label: &str, tailer: &UsageTailer, cpu: f64, wall: f64, parses_before: usize) -> Vec<UsageEvent> {
    let events = tailer.events.lock().clone();
    println!(
        "{label}\tcpu_ms={cpu:.0}\twall_ms={wall:.0}\tparsed={}\tevents={}\tlanes={}\tdigest={}",
        tailer.parse_count() - parses_before,
        events.len(),
        lanes(&events),
        digest(&events)
    );
    events
}

#[test]
#[ignore]
fn bench_tail_tick() {
    let _serial = BENCH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // The app's pool size; set before anything uses rayon.
    std::sync::LazyLock::force(&crate::RAYON_INIT);
    let env = BenchEnv::read();
    // Appends are stamped 1..=ticks seconds back, all inside the 1 h window.
    let ticks = count("BENCH_TICKS", setting("BENCH_TICKS").as_deref(), 20, 1..3600)
        .unwrap_or_else(|e| panic!("{e}"));
    env.claim();
    let clock = || env.now;
    let mut failures = Vec::new();

    let (c0, w0) = (cpu_ms(), Instant::now());
    let parsed = tokscale_core::parse_local_clients(env.context.parse_options(None, None));
    println!(
        "warmup\tcpu_ms={:.0}\twall_ms={:.0}\tmessages={}",
        cpu_ms() - c0,
        w0.elapsed().as_secs_f64() * 1000.0,
        parsed.as_ref().map(|p| p.messages.len()).unwrap_or(0)
    );
    if let Err(e) = &parsed {
        failures.push(format!("warmup parse failed: {e}"));
    }

    let tailer = UsageTailer::new();
    let (c, w) = (cpu_ms(), Instant::now());
    tailer.tick_with(&env.context, clock);
    let first = report("tick0", &tailer, cpu_ms() - c, w.elapsed().as_secs_f64() * 1000.0, 0);
    // The window is the point of the run: an empty one compares nothing.
    if first.is_empty() {
        failures.push("the tick0 window is empty".into());
    }

    let lane_len = |events: &[UsageEvent]| events.iter().filter(|e| e.client == env.kind).count();
    let mut prev_lane = lane_len(&first);
    let mut cpus = Vec::with_capacity(ticks);
    for i in 1..=ticks {
        let label = format!("tick{i}");
        env.append(i, (ticks - i + 1) as i64);
        let parses_before = tailer.parse_count();
        let (c, w) = (cpu_ms(), Instant::now());
        tailer.tick_with(&env.context, clock);
        let cpu = cpu_ms() - c;
        let events = report(&label, &tailer, cpu, w.elapsed().as_secs_f64() * 1000.0, parses_before);
        if env.append.is_some() {
            let ok = lane_len(&events) > prev_lane;
            println!("grow\t{label}\t{}", if ok { "ok" } else { "FAIL" });
            if !ok {
                failures.push(format!("grow {label}"));
            }
        }
        prev_lane = lane_len(&events);
        cpus.push(cpu);
    }
    cpus.sort_by(f64::total_cmp);
    println!(
        "summary\tmode={}\tticks={ticks}\tmedian_cpu_ms={:.0}\tmax_cpu_ms={:.0}\tparses={}",
        if env.append.is_some() { "append" } else { "unchanged" },
        cpus[cpus.len() / 2],
        cpus[cpus.len() - 1],
        tailer.parse_count()
    );
    // Unchanged mode measures the skip path; a token that keeps moving would
    // quietly turn every tick into a full parse.
    if env.append.is_none() && tailer.parse_count() != 1 {
        failures.push(format!("unchanged mode parsed {} times, expected 1", tailer.parse_count()));
    }
    assert!(failures.is_empty(), "failed checks: {failures:?}");
}

/// Cost of one graph recompute: the token probe and `usage_graph::run`, as
/// `graph_compute` runs them, each bracketed by getrusage. Every iteration
/// first appends a synthesized line and checks that the token moved, as it does
/// while an agent is writing; without that, the app would serve its cached
/// graph and there is no recompute to time.
#[test]
#[ignore]
fn bench_graph_recompute() {
    let _serial = BENCH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::sync::LazyLock::force(&crate::RAYON_INIT);
    let env = BenchEnv::read();
    assert!(env.append.is_some(), "the graph bench needs BENCH_APPEND");
    let iters = count("BENCH_GRAPH_ITERS", setting("BENCH_GRAPH_ITERS").as_deref(), 10, 1..3600)
        .unwrap_or_else(|e| panic!("{e}"));
    env.claim();
    let mut failures = Vec::new();
    let probe = || tokscale_core::local_source_change_token(&env.context.parse_options(None, None));

    // Warm the cache once so every measured iteration is a warm recompute.
    let (c, w) = (cpu_ms(), Instant::now());
    let warm = crate::usage_graph::run(&env.context, "");
    println!(
        "graph-warmup\tcpu_ms={:.0}\twall_ms={:.0}\tok={}",
        cpu_ms() - c,
        w.elapsed().as_secs_f64() * 1000.0,
        warm.is_ok()
    );
    if let Err(e) = &warm {
        failures.push(format!("warmup: {e}"));
    }
    let mut prev_token = match probe() {
        Ok(t) => Some(t),
        Err(e) => {
            failures.push(format!("token probe after warmup: {e}"));
            None
        }
    };

    let (mut tokens, mut runs) = (Vec::new(), Vec::new());
    for i in 1..=iters {
        env.append(i, (iters - i + 1) as i64);
        let c = cpu_ms();
        let token = probe();
        let token_cpu = cpu_ms() - c;
        match &token {
            Err(e) => failures.push(format!("iter{i} token probe: {e}")),
            Ok(t) if Some(*t) == prev_token => failures.push(format!("iter{i}: the append did not move the token")),
            Ok(_) => {}
        }
        prev_token = token.ok();
        let (c, w) = (cpu_ms(), Instant::now());
        let result = crate::usage_graph::run(&env.context, "");
        let run_cpu = cpu_ms() - c;
        println!(
            "graph\titer{i}\ttoken_cpu_ms={token_cpu:.0}\trun_cpu_ms={run_cpu:.0}\trun_wall_ms={:.0}\tok={}",
            w.elapsed().as_secs_f64() * 1000.0,
            result.is_ok()
        );
        if let Err(e) = &result {
            failures.push(format!("iter{i}: {e}"));
        }
        tokens.push(token_cpu);
        runs.push(run_cpu);
    }
    for v in [&mut tokens, &mut runs] {
        v.sort_by(f64::total_cmp);
    }
    println!(
        "graph-summary\titers={iters}\ttoken_median_ms={:.0}\ttoken_max_ms={:.0}\trun_median_ms={:.0}\trun_max_ms={:.0}",
        tokens[iters / 2],
        tokens[iters - 1],
        runs[iters / 2],
        runs[iters - 1],
    );
    assert!(failures.is_empty(), "failed recomputes: {failures:?}");
}

#[test]
fn digest_sees_every_field_and_ignores_order() {
    let event = |ts, client: &str| UsageEvent {
        ts_ms: ts,
        client: client.into(),
        agent: "Main".into(),
        model: "m".into(),
        input: 10,
        output: 1,
        reasoning: 0,
        cache_read: 0,
        cache_write: 0,
        message_count: 1,
    };
    let events = vec![event(1, "claude"), event(2, "codex"), event(2, "grok")];
    let base = digest(&events);
    let edit = |f: &dyn Fn(&mut UsageEvent)| {
        let mut v = events.clone();
        f(&mut v[0]);
        digest(&v)
    };
    let mut removed = events.clone();
    removed.remove(0);
    let mut duplicated = events.clone();
    duplicated.push(events[0].clone());
    let mut reordered = events.clone();
    reordered.reverse();
    for (name, changed) in [
        ("remove one", digest(&removed)),
        ("duplicate one", digest(&duplicated)),
        ("ts", edit(&|e| e.ts_ms += 1)),
        ("client", edit(&|e| e.client.push('x'))),
        ("agent", edit(&|e| e.agent.push('x'))),
        ("model", edit(&|e| e.model.push('x'))),
        ("input", edit(&|e| e.input += 1)),
        ("output", edit(&|e| e.output += 1)),
        ("reasoning", edit(&|e| e.reasoning += 1)),
        ("cache_read", edit(&|e| e.cache_read += 1)),
        ("cache_write", edit(&|e| e.cache_write += 1)),
        ("message_count", edit(&|e| e.message_count += 1)),
    ] {
        assert_ne!(changed, base, "{name}");
    }
    assert_eq!(digest(&reordered), base, "reorder");
}

#[test]
fn append_record_keeps_records_on_separate_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (before, after) in [("a", "a\nb\n"), ("a\n", "a\nb\n"), ("", "b\n")] {
        let path = dir.path().join("f.jsonl");
        std::fs::write(&path, before).expect("write");
        append_record(&path, "b");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), after, "{before:?}");
    }
}

#[test]
fn source_line_is_chosen_by_fields_and_survives_bad_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("s.jsonl");
    let mut bytes = Vec::new();
    for line in [
        r#"{"type":"assistant","message":{"id":"m1","model":"claude-x","usage":{"input_tokens":1}}}"#,
        r#"{"message": {"usage": {"input_tokens": 2}, "model": "claude-x", "id": "m2"}, "type": "assistant"}"#,
        r#"{"type":"user","message":{"usage":{"input_tokens":3}}}"#,
        r#"{"type":"assistant","message":{"id":"m4","model":"<synthetic>","usage":{"input_tokens":4}}}"#,
        r#"{"type":"assistant","message":{"id":"m5","usage":{"input_tokens":5}}}"#,
    ] {
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
    }
    // A line that is not UTF-8 must be skipped, not end the scan before m6.
    bytes.extend_from_slice(b"{\"usage\": \"\xff\xfe\"}\n");
    bytes.extend_from_slice(
        br#"{"type":"assistant","message":{"id":"m6","model":"claude-x","usage":{"input_tokens":6}}}"#,
    );
    std::fs::write(&path, bytes).expect("write");
    let line = last_line_with(&path, &["\"usage\""], is_claude_source);
    assert_eq!(line["message"]["id"], "m6", "{line}");
}

#[test]
fn corpus_guard_refuses_what_would_touch_real_data() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus = dir.path().join("c");
    std::fs::create_dir_all(corpus.join("home")).expect("home");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(outside.join("cache")).expect("outside cache");
    std::fs::create_dir_all(corpus.join("cfg")).expect("cfg");
    let cfg_path = corpus.join("cfg");
    let (cfg, outside_s) = (cfg_path.to_str().expect("utf8"), outside.to_str().expect("utf8"));
    let check = |c: Option<&str>, p: Option<&str>| check_corpus(&corpus, c, p);

    assert!(check(Some(cfg), Some("1")).unwrap_err().contains("no IDENTITY"));
    std::fs::write(corpus.join("IDENTITY"), b"").expect("identity");
    assert!(check(None, Some("1")).unwrap_err().contains("TOKSCALE_CONFIG_DIR"));
    assert!(check(Some(outside_s), Some("1")).unwrap_err().contains("outside the corpus"));
    // A cache subdirectory linked out of the corpus is refused too.
    std::os::unix::fs::symlink(outside.join("cache"), cfg_path.join("cache")).expect("link cache");
    assert!(check(Some(cfg), Some("1")).unwrap_err().contains("outside the corpus"));
    std::fs::remove_file(cfg_path.join("cache")).expect("unlink cache");
    std::fs::create_dir_all(cfg_path.join("cache")).expect("cache");
    assert!(check(Some(cfg), None).unwrap_err().contains("PRICING_CACHE_ONLY"));
    assert!(check(Some(cfg), Some("1")).unwrap_err().contains("pricing-litellm"));
    std::fs::write(cfg_path.join("cache/pricing-litellm.json"), b"{}").expect("pricing");
    let (_, home) = check(Some(cfg), Some("1")).expect("a fresh corpus is accepted");
    assert!(home.ends_with("home"));
    std::fs::write(corpus.join("BENCH_USED"), b"").expect("marker");
    assert!(check(Some(cfg), Some("1")).unwrap_err().contains("already used"));
}

#[test]
fn append_target_must_be_a_single_link_file_under_home() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join("p")).expect("home");
    let home = home.canonicalize().expect("canonical home");
    std::fs::write(home.join("p/s.jsonl"), b"x\n").expect("session");
    std::fs::write(dir.path().join("real.jsonl"), b"x\n").expect("real");

    assert!(check_append(&home, "p/s.jsonl").is_ok());
    assert!(check_append(&home, "../real.jsonl").unwrap_err().contains("not a file under home"));
    let real = dir.path().join("real.jsonl");
    assert!(check_append(&home, real.to_str().expect("utf8")).unwrap_err().contains("not a file under home"));
    std::fs::hard_link(dir.path().join("real.jsonl"), home.join("p/linked.jsonl")).expect("hard link");
    assert!(check_append(&home, "p/linked.jsonl").unwrap_err().contains("hard links"));
}

#[test]
fn counts_reject_typos_zero_and_out_of_range() {
    assert_eq!(count("N", None, 20, 1..3600), Ok(20));
    assert_eq!(count("N", Some("5"), 20, 1..3600), Ok(5));
    for bad in ["2x", "1e3", "-5", "0", "3600"] {
        assert!(count("N", Some(bad), 20, 1..3600).is_err(), "{bad}");
    }
}
