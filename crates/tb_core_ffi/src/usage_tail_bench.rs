//! Benchmarks for the live tail and the graph recompute on a captured corpus.
//! Run one bench per invocation, from the repository root:
//!
//! ```text
//! TOKSCALE_CONFIG_DIR=<corpus>/cfg TOKSCALE_PRICING_CACHE_ONLY=1 RAYON_NUM_THREADS=2 \
//!   BENCH_CORPUS=<corpus> BENCH_NOW_MS=<capture time in ms> \
//!   cargo test -p tb_core_ffi --release --lib usage_tail::bench::bench_tail_tick \
//!   -- --exact --ignored --nocapture
//! ```
//!
//! The corpus is a directory holding `IDENTITY`, `home/` (copied session data,
//! mtimes preserved — the tail prunes by mtime) and `cfg/` (the message cache
//! and a pricing cache). The bench reads `home/` through an injected context
//! that ignores per-client root variables (CODEX_HOME and the like), so the
//! process HOME stays real for the toolchain. It refuses a corpus that is
//! malformed or already used: a run writes the message cache and, in append
//! mode, a session file, so each run needs a fresh copy (it leaves a
//! `BENCH_USED` marker). RAYON_NUM_THREADS=2 matches the app's pool.
//!
//!   BENCH_CORPUS       the corpus directory (required)
//!   BENCH_NOW_MS       fixed clock in Unix milliseconds (required), normally
//!                      the capture time
//!   BENCH_TICKS        ticks after the first, 1..3599 (default 20)
//!   BENCH_GRAPH_ITERS  graph recomputes, at least 1 (default 10)
//!   BENCH_APPEND       file under `home/` (relative) to append one synthesized
//!                      line to per tick; required by the graph bench
//!   BENCH_APPEND_KIND  `claude` (default) or `codex`
//!
//! A synthesized line carries a fresh per-run identity (Claude ids; Codex
//! cumulative totals), because re-appending an existing line is dropped by
//! dedup and would never add an event. Its timestamp is not always the one
//! written: a Claude assistant line right after a user or tool_result line
//! takes that line's request start, and a Codex token_count takes the previous
//! accepted token_count's time — so the target's last real record should be
//! inside the window. A degenerate run fails rather than reporting timings:
//! an empty tick0 window, an append that adds no event to its lane, unchanged
//! mode that parsed more than once, a graph append that did not move the token,
//! or a failed recompute.
//!
//! Limits: the digest covers the tail's event window, not `rate_in_window` or
//! `trace` (they still read the wall clock). The graph bench measures cost only
//! (no payload digest), and times one token probe per recompute where the
//! app's `tb_graph` makes two (`graph_cached`, then `graph_compute`). CPU comes
//! from a hand-declared getrusage with Darwin's layout, hence macOS only.
use super::{UsageEvent, UsageTailer};
use crate::LocalSourceContext;
use chrono::{SecondsFormat, TimeZone, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
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
fn synthesize(source: &Value, kind: &str, id: &str, n: i64, ts_ms: i64) -> String {
    let mut v = source.clone();
    v["timestamp"] = Utc
        .timestamp_millis_opt(ts_ms)
        .single()
        .expect("timestamp")
        .to_rfc3339_opts(SecondsFormat::Millis, true)
        .into();
    if kind == "claude" {
        v["uuid"] = format!("bench-uuid-{id}").into();
        v["requestId"] = format!("req_bench_{id}").into();
        v["message"]["id"] = format!("msg_bench_{id}").into();
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
/// pricing settings point into; returns its `home/`. Pure checks, so the
/// guard is unit-tested rather than living only inside `#[ignore]` benches.
fn check_corpus(corpus: &Path, config_dir: Option<&str>, pricing_cache_only: Option<&str>) -> Result<PathBuf, String> {
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
    if !cfg.starts_with(&corpus) {
        return Err(format!("TOKSCALE_CONFIG_DIR {} is outside the corpus", cfg.display()));
    }
    if pricing_cache_only != Some("1") {
        return Err("TOKSCALE_PRICING_CACHE_ONLY must be 1, or a recompute may fetch prices".into());
    }
    Ok(home)
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

/// Validated settings for one bench run.
struct BenchEnv {
    context: LocalSourceContext,
    home: PathBuf,
    now: i64,
    append: Option<(PathBuf, Value)>,
    kind: String,
    /// Salts synthesized ids so they are unique to this run.
    run: String,
}

impl BenchEnv {
    fn read() -> Self {
        let var = |k: &str| std::env::var(k).ok();
        let corpus = var("BENCH_CORPUS").expect("BENCH_CORPUS");
        let home = check_corpus(
            Path::new(&corpus),
            var("TOKSCALE_CONFIG_DIR").as_deref(),
            var("TOKSCALE_PRICING_CACHE_ONLY").as_deref(),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        // Claim the corpus before any write, so a crashed run cannot be reused.
        std::fs::write(Path::new(&corpus).join("BENCH_USED"), b"").expect("mark corpus used");

        let now_raw = var("BENCH_NOW_MS").expect("BENCH_NOW_MS (a fixed clock is required)");
        let now: i64 = now_raw
            .parse()
            .unwrap_or_else(|_| panic!("BENCH_NOW_MS {now_raw:?} is not an integer"));
        // 2001..2286 in milliseconds: rejects seconds, micro- and nanoseconds.
        assert!(
            (1_000_000_000_000..10_000_000_000_000).contains(&now),
            "BENCH_NOW_MS {now} is not a millisecond timestamp"
        );
        let kind = var("BENCH_APPEND_KIND").unwrap_or_else(|| "claude".into());
        assert!(matches!(kind.as_str(), "claude" | "codex"), "BENCH_APPEND_KIND {kind}");
        let append = var("BENCH_APPEND").map(|rel| {
            assert!(!Path::new(&rel).is_absolute(), "BENCH_APPEND {rel} must be relative to home/");
            let file = home.join(&rel).canonicalize().expect("canonicalize BENCH_APPEND");
            assert!(file.is_file() && file.starts_with(&home), "BENCH_APPEND {rel} is not a file under home/");
            let source = match kind.as_str() {
                "claude" => last_line_with(&file, &["\"usage\""], is_claude_source),
                _ => last_line_with(&file, &["\"token_count\"", "\"total_token_usage\""], |_| true),
            };
            (file, source)
        });
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        println!("clock\tnow_ms={now}\tcorpus={}", home.display());
        Self {
            context: LocalSourceContext::for_corpus(home.clone()),
            home,
            now,
            append,
            kind,
            run: format!("{:x}{:x}", std::process::id(), nanos),
        }
    }

    /// Appends the `n`-th synthesized line, stamped `back_secs` before the clock.
    fn append(&self, n: usize, back_secs: i64) {
        if let Some((file, source)) = &self.append {
            let id = format!("{}-{n}", self.run);
            let line = synthesize(source, &self.kind, &id, n as i64, self.now - back_secs * 1000);
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
    let env = BenchEnv::read();
    // Appends are stamped 1..=ticks seconds back, all inside the 1 h window.
    let ticks = count("BENCH_TICKS", std::env::var("BENCH_TICKS").ok().as_deref(), 20, 1..3600)
        .unwrap_or_else(|e| panic!("{e}"));
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

/// Cost of one graph recompute, split the way `graph_compute` runs it (token
/// probe, then `usage_graph::run`), each bracketed by getrusage. Every
/// iteration first appends a synthesized line and checks that the token moved,
/// as it does while an agent is writing; without that, the app would serve its
/// cached graph and there is no recompute to time.
#[test]
#[ignore]
fn bench_graph_recompute() {
    let _serial = BENCH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = BenchEnv::read();
    assert!(env.append.is_some(), "the graph bench needs BENCH_APPEND");
    let iters = count("BENCH_GRAPH_ITERS", std::env::var("BENCH_GRAPH_ITERS").ok().as_deref(), 10, 1..3600)
        .unwrap_or_else(|e| panic!("{e}"));
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
    let mut prev_token = probe().ok();

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
        "graph-summary\titers={iters}\ttoken_median_ms={:.0}\ttoken_max_ms={:.0}\trun_median_ms={:.0}\trun_max_ms={:.0}\tcorpus={}",
        tokens[iters / 2],
        tokens[iters - 1],
        runs[iters / 2],
        runs[iters - 1],
        env.home.display()
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
    std::fs::create_dir_all(corpus.join("cfg")).expect("cfg");
    let outside = dir.path().join("outside-cfg");
    std::fs::create_dir_all(&outside).expect("outside");
    let cfg = corpus.join("cfg");
    let (cfg, outside) = (cfg.to_str().expect("utf8"), outside.to_str().expect("utf8"));
    let check = |c: Option<&str>, p: Option<&str>| check_corpus(&corpus, c, p);

    assert!(check(Some(cfg), Some("1")).unwrap_err().contains("no IDENTITY"));
    std::fs::write(corpus.join("IDENTITY"), b"").expect("identity");
    assert!(check(None, Some("1")).unwrap_err().contains("TOKSCALE_CONFIG_DIR"));
    assert!(check(Some(outside), Some("1")).unwrap_err().contains("outside the corpus"));
    assert!(check(Some(cfg), None).unwrap_err().contains("PRICING_CACHE_ONLY"));
    let home = check(Some(cfg), Some("1")).expect("a fresh corpus is accepted");
    assert!(home.ends_with("home"));
    std::fs::write(corpus.join("BENCH_USED"), b"").expect("marker");
    assert!(check(Some(cfg), Some("1")).unwrap_err().contains("already used"));
}

#[test]
fn counts_reject_typos_zero_and_out_of_range() {
    assert_eq!(count("N", None, 20, 1..3600), Ok(20));
    assert_eq!(count("N", Some("5"), 20, 1..3600), Ok(5));
    for bad in ["2x", "1e3", "-5", "0", "3600"] {
        assert!(count("N", Some(bad), 20, 1..3600).is_err(), "{bad}");
    }
}
